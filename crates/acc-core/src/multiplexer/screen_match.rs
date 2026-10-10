//! Reading a harness's state from its rendered TUI screen.
//!
//! For a session ACC didn't start, the screen is sometimes the only place
//! some state shows: Claude Code writes nothing to its transcript while a
//! permission dialog waits, and pi before 1.x sends no OSC 7501. [`read`]
//! matches the text a multiplexer prints for a pane (`tmux capture-pane -p`,
//! `zellij action dump-screen`, or a zellij `subscribe` viewport) and
//! [`answer`] turns a choice into the keys that pick it.
//!
//! Screens are matched on layout, not exact wording, and were checked
//! against Claude Code 2.1.296 and pi 0.82.1 and 1.1.0 (fixtures in
//! `tests/fixtures/screens`). Observations carry [`StateSource::ScreenMatch`],
//! so a transcript or OSC 7501 report wins over them while it is fresh.
//!
//! tmux and screen stream raw bytes rather than a rendered screen: capture
//! the pane again whenever output arrives (debounced) and feed the result to
//! a [`ScreenObserver`].

use serde::{Deserialize, Serialize};

use crate::driver::DriverError;
use crate::event::{Event, PermissionOption};
use crate::harness::Harness;
use crate::state::{BlockKind, RunState, SessionState, StateSource};

use super::{Input, Key};

/// What a screen shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenRead {
    pub state: RunState,
    /// The spinner text while working (`Working...`, `Accomplishing…`).
    pub activity: Option<String>,
    /// The dialog waiting for an answer, when blocked.
    pub dialog: Option<Dialog>,
}

/// A dialog waiting for the user to pick an option.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dialog {
    /// Stable for as long as the same dialog is open, whatever the pane
    /// width; pass it back to [`answer`].
    pub id: String,
    pub kind: BlockKind,
    /// First line of the dialog (`Bash command`, `Create file`, a question header).
    pub title: String,
    /// Lines between the title and the question (the command, the file, a diff).
    pub detail: Vec<String>,
    /// `Do you want to proceed?` and the like.
    pub question: String,
    pub options: Vec<DialogOption>,
    /// Whether the options are numbered, so a digit picks one directly.
    pub numbered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DialogOption {
    /// The number shown on screen, or the 1-based position when unnumbered.
    pub id: String,
    pub label: String,
    /// Wrapped or descriptive lines under the label.
    pub detail: Option<String>,
    /// Under the selection cursor.
    pub selected: bool,
}

/// Option id that dismisses a dialog with Escape (a deny for permissions).
pub const CANCEL: &str = "cancel";

impl Dialog {
    /// One line describing the request, for [`Event::PermissionRequested`].
    pub fn summary(&self) -> String {
        let mut parts = vec![self.title.as_str()];
        parts.extend(self.detail.iter().map(String::as_str));
        if !self.question.is_empty() {
            parts.push(&self.question);
        }
        parts.join(" · ")
    }

    pub fn permission_event(&self) -> Event {
        Event::PermissionRequested {
            request_id: self.id.clone(),
            title: self.summary(),
            options: self
                .options
                .iter()
                .map(|o| PermissionOption {
                    id: o.id.clone(),
                    label: o.label.clone(),
                })
                .chain([PermissionOption {
                    id: CANCEL.into(),
                    label: "Cancel (Esc)".into(),
                }])
                .collect(),
        }
    }
}

impl ScreenRead {
    pub fn session_state(&self, now_ms: u64) -> SessionState {
        SessionState {
            state: self.state.clone(),
            message: match &self.dialog {
                Some(d) => Some(d.question.clone()).filter(|q| !q.is_empty()),
                None => self.activity.clone(),
            },
            source: StateSource::ScreenMatch,
            observed_at_ms: now_ms,
        }
    }
}

/// Whether ACC can read `harness`'s screen.
pub fn supported(harness: Harness) -> bool {
    matches!(harness, Harness::Claude | Harness::Pi)
}

/// Reads `screen` as `harness`'s TUI. `None` when the harness has no screen
/// matcher or the screen doesn't look like its TUI.
pub fn read(harness: Harness, screen: &str) -> Option<ScreenRead> {
    let lines: Vec<&str> = screen.lines().map(str::trim_end).collect();
    match harness {
        Harness::Claude => claude(&lines),
        Harness::Pi => pi(&lines),
        _ => None,
    }
}

/// Keys that pick `option_id` in the dialog `request_id`, read from the
/// current `screen`. Reading the screen again guards against answering a
/// dialog that changed since it was reported.
pub fn answer(
    harness: Harness,
    screen: &str,
    request_id: &str,
    option_id: &str,
) -> Result<Vec<Input>, DriverError> {
    if !supported(harness) {
        return Err(DriverError::Unsupported {
            harness,
            what: "reading dialogs from the screen",
        });
    }
    let dialog = read(harness, screen)
        .and_then(|r| r.dialog)
        .filter(|d| d.id == request_id)
        .ok_or_else(|| DriverError::UnknownRequest(request_id.to_owned()))?;
    if option_id == CANCEL {
        return Ok(vec![Input::Keys(vec![Key::Escape])]);
    }
    let target = dialog
        .options
        .iter()
        .position(|o| o.id == option_id)
        .ok_or_else(|| DriverError::InvalidOption(option_id.to_owned()))?;
    if dialog.numbered
        && let [digit] = option_id.as_bytes()
        && digit.is_ascii_digit()
    {
        return Ok(vec![Input::Keys(vec![Key::Char(char::from(*digit))])]);
    }
    let from = dialog.options.iter().position(|o| o.selected).unwrap_or(0);
    let step = if target > from { Key::Down } else { Key::Up };
    let mut keys = vec![step; target.abs_diff(from)];
    keys.push(Key::Enter);
    Ok(vec![Input::Keys(keys)])
}

/// Turns successive screens of one pane into events, reporting only changes:
/// [`Event::StateChanged`], and [`Event::PermissionRequested`] /
/// [`Event::PermissionResolved`] as dialogs open and close.
#[derive(Debug)]
pub struct ScreenObserver {
    harness: Harness,
    last: Option<(RunState, Option<String>)>,
    dialog: Option<String>,
}

impl ScreenObserver {
    pub fn new(harness: Harness) -> Self {
        Self {
            harness,
            last: None,
            dialog: None,
        }
    }

    /// The id of the dialog open on the last screen.
    pub fn open_dialog(&self) -> Option<&str> {
        self.dialog.as_deref()
    }

    /// Feeds the pane's current screen. Unrecognized screens change nothing.
    pub fn feed(&mut self, screen: &str, now_ms: u64) -> Vec<Event> {
        let Some(read) = read(self.harness, screen) else {
            return Vec::new();
        };
        let mut events = Vec::new();
        let id = read.dialog.as_ref().map(|d| d.id.clone());
        if id != self.dialog {
            if let Some(old) = self.dialog.take() {
                events.push(Event::PermissionResolved { request_id: old });
            }
            if let Some(d) = &read.dialog {
                events.push(d.permission_event());
            }
            self.dialog = id;
        }
        let state = read.session_state(now_ms);
        let key = (state.state.clone(), state.message.clone());
        if self.last.as_ref() != Some(&key) {
            self.last = Some(key);
            events.push(Event::StateChanged(state));
        }
        events
    }
}

fn indent(line: &str) -> usize {
    line.chars().take_while(|c| c.is_whitespace()).count()
}

/// A horizontal rule drawn with `ch`.
fn is_rule(line: &str, ch: char, min: usize) -> bool {
    let t = line.trim();
    t.chars().count() >= min && t.chars().all(|c| c == ch)
}

/// The text after a selection cursor: `❯ ` or, where Claude can't draw
/// that (some zellij setups), `> `.
fn after_cursor(line: &str) -> Option<&str> {
    let t = line.trim_start();
    t.strip_prefix('❯')
        .or_else(|| t.strip_prefix("> "))
        .or_else(|| (t == ">").then_some(""))
        .map(str::trim_start)
}

/// `1. Yes` or `❯ 1. Yes`: (selected, number, label).
fn numbered_option(line: &str) -> Option<(bool, &str, &str)> {
    let (selected, t) = match after_cursor(line) {
        Some(rest) => (true, rest),
        None => (false, line.trim_start()),
    };
    let digits = t.find(|c: char| !c.is_ascii_digit())?;
    let rest = t[digits..].strip_prefix(". ")?;
    (digits > 0).then(|| (selected, &t[..digits], rest.trim()))
}

/// FNV-1a over the dialog's text in screen order with all whitespace
/// removed, so rewrapping it at another width keeps the id.
fn dialog_id<'a>(text: impl IntoIterator<Item = &'a str>, options: &'a [DialogOption]) -> String {
    let options = options
        .iter()
        .flat_map(|o| [o.label.as_str(), o.detail.as_deref().unwrap_or("")]);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in text.into_iter().chain(options) {
        for b in part.bytes().filter(|b| !b.is_ascii_whitespace()) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("screen-{h:016x}")
}

/// The last non-empty line's index.
fn last_content(lines: &[&str]) -> Option<usize> {
    lines.iter().rposition(|l| !l.trim().is_empty())
}

fn claude(lines: &[&str]) -> Option<ScreenRead> {
    let last = last_content(lines)?;
    // The footer (`Enter to confirm · Esc to cancel`) wraps in narrow panes.
    let footer = if lines[last].contains("Esc to cancel") {
        Some(last)
    } else {
        last_content(&lines[..last])
            .filter(|&p| format!("{} {}", lines[p].trim(), lines[last].trim()).contains("Esc to cancel"))
    };
    if let Some(footer) = footer
        && let Some(dialog) = claude_dialog(lines, footer)
    {
        return Some(ScreenRead {
            state: RunState::Blocked { kind: dialog.kind },
            activity: None,
            dialog: Some(dialog),
        });
    }
    // The prompt box and its footer sit at the bottom; the spinner line
    // (`✽ Accomplishing…`) just above the box.
    let tail = &lines[last.saturating_sub(8)..=last];
    let activity = tail.iter().find_map(|l| claude_spinner(l));
    if activity.is_some() || tail.iter().any(|l| l.contains("esc to interrupt")) {
        return Some(ScreenRead {
            state: RunState::Working { progress: None },
            activity,
            dialog: None,
        });
    }
    let has_prompt = tail.iter().any(|l| after_cursor(l).is_some());
    let has_box = tail.iter().filter(|l| is_rule(l, '─', 10)).count() >= 2;
    (has_prompt && has_box).then_some(ScreenRead {
        state: RunState::Idle,
        activity: None,
        dialog: None,
    })
}

/// `✽ Accomplishing… (12s · esc to interrupt)` → `Accomplishing…`. A finished
/// turn's line (`✻ Churned for 3s`) has no ellipsis on its verb.
fn claude_spinner(line: &str) -> Option<String> {
    let mut chars = line.chars();
    let glyph = chars.next()?;
    if !matches!(glyph, '·' | '✢' | '✳' | '✶' | '✻' | '✽' | '*') {
        return None;
    }
    let rest = chars.as_str().strip_prefix(' ')?;
    let verb = rest.split([' ', '(']).next()?;
    (verb.ends_with('…') && verb.len() > '…'.len_utf8()).then(|| verb.to_owned())
}

fn claude_dialog(lines: &[&str], footer: usize) -> Option<Dialog> {
    let (options, numbered, above) =
        numbered_options(lines, footer).or_else(|| cursor_options(lines, footer))?;

    // The dialog starts below a full-width rule.
    let top = lines[..=above]
        .iter()
        .rposition(|l| is_rule(l, '─', 10))
        .map_or(0, |i| i + 1);
    let block: Vec<String> = lines[top..=above]
        .iter()
        .filter(|l| !l.trim().is_empty() && !is_rule(l, '╌', 3))
        .map(|l| l.trim().to_owned())
        .collect();
    let (title, rest) = block.split_first()?;
    let (question, detail) = match rest.iter().rposition(|l| l.contains('?')) {
        Some(q) => {
            let mut detail = rest.to_vec();
            (detail.remove(q), detail)
        }
        None => (String::new(), rest.to_vec()),
    };
    // AskUserQuestion shows its header as a tab: `☐ Beverage`.
    let (title, kind) = match title.strip_prefix(['☐', '☒', '✔']) {
        Some(t) => (t.trim().to_owned(), BlockKind::Question),
        None if question.starts_with("Do you want to") || title.starts_with("Accessing workspace") => {
            (title.clone(), BlockKind::Permission)
        }
        None => (title.clone(), BlockKind::Question),
    };
    Some(Dialog {
        id: dialog_id(block.iter().map(String::as_str), &options),
        kind,
        title,
        detail,
        question,
        options,
        numbered,
    })
}

/// Numbered options ending just above `footer`: the options, `true`, and the
/// index of the last line above them. Indented lines under an option are
/// its wrapped label or description.
fn numbered_options(lines: &[&str], footer: usize) -> Option<(Vec<DialogOption>, bool, usize)> {
    let mut options = Vec::new();
    let mut pending: Vec<&str> = Vec::new();
    let mut above = None;
    for i in (0..footer).rev() {
        let line = lines[i];
        if line.trim().is_empty() || is_rule(line, '─', 3) {
            continue;
        }
        if let Some((selected, id, label)) = numbered_option(line) {
            pending.reverse();
            let detail = (!pending.is_empty()).then(|| pending.join(" "));
            pending.clear();
            options.push(DialogOption {
                id: id.to_owned(),
                label: label.to_owned(),
                detail,
                selected,
            });
        } else if indent(line) >= 4 {
            pending.push(line.trim());
        } else {
            above = Some(i);
            break;
        }
    }
    if options.is_empty() {
        return None;
    }
    options.reverse();
    Some((options, true, above?))
}

/// Unnumbered options marked by a `❯` cursor (the folder trust dialog):
/// the cursor line plus neighbours indented to its label's column. Labels
/// start with a capital, so a lowercase line continues the one above.
fn cursor_options(lines: &[&str], footer: usize) -> Option<(Vec<DialogOption>, bool, usize)> {
    let start = lines[..footer].iter().rposition(|l| !l.trim().is_empty())?;
    let mut first = start;
    while first > 0 && !lines[first - 1].trim().is_empty() {
        first -= 1;
    }
    let block = &lines[first..=start];
    let cursor = block.iter().position(|l| after_cursor(l).is_some())?;
    let col = {
        let l = block[cursor];
        l.chars().count() - after_cursor(l)?.chars().count()
    };
    let is_option = |l: &&str| after_cursor(l).is_some() || indent(l) == col;
    let mut lo = cursor;
    while lo > 0 && is_option(&block[lo - 1]) {
        lo -= 1;
    }
    let mut hi = cursor;
    while hi + 1 < block.len() && is_option(&block[hi + 1]) {
        hi += 1;
    }
    let mut options: Vec<DialogOption> = Vec::new();
    for l in &block[lo..=hi] {
        let cursor = after_cursor(l);
        let text = cursor.unwrap_or(l).trim();
        if let Some(prev) = options.last_mut()
            && cursor.is_none()
            && text.starts_with(char::is_lowercase)
        {
            prev.label = format!("{} {text}", prev.label);
            continue;
        }
        options.push(DialogOption {
            id: (options.len() + 1).to_string(),
            label: text.to_owned(),
            detail: None,
            selected: cursor.is_some(),
        });
    }
    let above = (first + lo).checked_sub(1)?;
    Some((options, false, above))
}

/// pi draws its spinner on the line above the editor (0.82: `⠧ Working...`)
/// or inside the editor's top border (1.1: `── ⠧ Working ──`).
fn pi(lines: &[&str]) -> Option<ScreenRead> {
    let last = last_content(lines)?;
    let tail = &lines[last.saturating_sub(12)..=last];
    let activity = tail.iter().find_map(|l| {
        let t = l.trim().trim_start_matches('─').trim_start();
        let mut chars = t.chars();
        let glyph = chars.next()?;
        let text = chars.as_str().strip_prefix(' ')?;
        let text = text.trim_end_matches('─').trim();
        (('\u{2801}'..='\u{28ff}').contains(&glyph) && !text.is_empty()).then(|| text.to_owned())
    });
    if activity.is_some() {
        return Some(ScreenRead {
            state: RunState::Working { progress: None },
            activity,
            dialog: None,
        });
    }
    tail.iter().any(|l| is_rule(l, '─', 20)).then_some(ScreenRead {
        state: RunState::Idle,
        activity: None,
        dialog: None,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    macro_rules! screen {
        ($name:literal) => {
            include_str!(concat!("../../tests/fixtures/screens/", $name, ".txt"))
        };
    }

    fn dialog(harness: Harness, screen: &str) -> Dialog {
        read(harness, screen).expect("read").dialog.expect("dialog")
    }

    fn labels(d: &Dialog) -> Vec<(&str, &str)> {
        d.options
            .iter()
            .map(|o| (o.id.as_str(), o.label.as_str()))
            .collect()
    }

    #[test]
    fn claude_bash_permission() {
        let r = read(Harness::Claude, screen!("claude-permission-bash")).unwrap();
        assert_eq!(
            r.state,
            RunState::Blocked {
                kind: BlockKind::Permission
            }
        );
        let d = r.dialog.clone().unwrap();
        assert_eq!(d.title, "Bash command");
        assert_eq!(d.question, "Do you want to proceed?");
        assert!(
            d.detail
                .iter()
                .any(|l| l == "touch /home/claude/scr/work/hello.txt")
        );
        assert!(d.numbered);
        assert_eq!(
            labels(&d),
            [
                ("1", "Yes"),
                (
                    "2",
                    "Yes, and always allow access to /home/claude/scr/work from this project"
                ),
                (
                    "3",
                    "Yes, and switch to auto mode · auto mode handles these prompts for you"
                ),
                ("4", "No"),
            ]
        );
        assert!(d.options[0].selected && !d.options[1].selected);
        assert_eq!(
            r.session_state(7).message.as_deref(),
            Some("Do you want to proceed?")
        );
    }

    #[test]
    fn claude_write_permission() {
        let d = dialog(Harness::Claude, screen!("claude-permission-write"));
        assert_eq!(d.kind, BlockKind::Permission);
        assert_eq!(d.title, "Create file");
        assert_eq!(d.question, "Do you want to create notes.md?");
        assert_eq!(d.detail, ["notes.md", "1 hi"]);
        assert_eq!(d.options.len(), 3);
        assert_eq!(d.options[2].label, "No");
    }

    #[test]
    fn narrow_pane_wraps_options_but_keeps_the_id() {
        let narrow = dialog(Harness::Claude, screen!("claude-permission-narrow"));
        assert_eq!(narrow.title, "Bash command");
        assert_eq!(labels(&narrow)[0], ("1", "Yes"));
        assert_eq!(narrow.options[1].label, "Yes, and always allow access to");
        assert_eq!(
            narrow.options[1].detail.as_deref(),
            Some("/home/claude/scr/work/some/fairly/long/neste d/directory/path from this project")
        );
        assert_eq!(narrow.options[3].label, "No");
        // The same dialog rendered at another width gets the same id.
        let wide = narrow
            .options
            .iter()
            .map(|o| format!("{}{}", o.label, o.detail.as_deref().unwrap_or("")))
            .collect::<Vec<_>>();
        let rewrapped: Vec<DialogOption> = wide
            .iter()
            .zip(&narrow.options)
            .map(|(label, o)| DialogOption {
                label: label.clone(),
                detail: None,
                ..o.clone()
            })
            .collect();
        let text = [narrow.title.as_str()]
            .into_iter()
            .chain(narrow.detail.iter().map(String::as_str))
            .chain([narrow.question.as_str()]);
        assert_eq!(dialog_id(text, &rewrapped), narrow.id);
    }

    #[test]
    fn claude_question() {
        let r = read(Harness::Claude, screen!("claude-question")).unwrap();
        assert_eq!(
            r.state,
            RunState::Blocked {
                kind: BlockKind::Question
            }
        );
        let d = r.dialog.unwrap();
        assert_eq!(d.title, "Beverage");
        assert_eq!(d.question, "Do you prefer tea or coffee?");
        assert_eq!(
            labels(&d),
            [
                ("1", "Tea"),
                ("2", "Coffee"),
                ("3", "Type something."),
                ("4", "Chat about this")
            ]
        );
        assert_eq!(d.options[0].detail.as_deref(), Some("You prefer tea."));
    }

    #[test]
    fn claude_trust_dialog_is_unnumbered() {
        let d = dialog(Harness::Claude, screen!("claude-trust"));
        assert_eq!(d.kind, BlockKind::Permission);
        assert_eq!(d.title, "Accessing workspace:");
        assert!(!d.numbered);
        assert_eq!(labels(&d), [("1", "No, exit"), ("2", "Yes, I trust this folder")]);
        assert!(d.options[0].selected);
        // Unnumbered: move the cursor, then Enter.
        assert_eq!(
            answer(Harness::Claude, screen!("claude-trust"), &d.id, "2").unwrap(),
            [Input::Keys(vec![Key::Down, Key::Enter])]
        );
    }

    #[test]
    fn narrow_ascii_trust_dialog() {
        // zellij at 24 columns, where Claude draws `>` for its cursor.
        let d = dialog(Harness::Claude, screen!("claude-trust-narrow-ascii"));
        assert_eq!(labels(&d), [("1", "No, exit"), ("2", "Yes, I trust this folder")]);
        assert!(d.options[0].selected);
    }

    #[test]
    fn claude_working_and_idle() {
        let w = read(Harness::Claude, screen!("claude-working")).unwrap();
        assert_eq!(w.state, RunState::Working { progress: None });
        assert_eq!(w.activity.as_deref(), Some("Accomplishing…"));
        assert_eq!(w.session_state(0).source, StateSource::ScreenMatch);
        let i = read(Harness::Claude, screen!("claude-idle")).unwrap();
        assert_eq!(i.state, RunState::Idle);
        assert_eq!(claude_spinner("✻ Churned for 3s · done 4:34 PM"), None);
        assert!(read(Harness::Claude, "$ ls\nfoo bar\n$").is_none());
    }

    #[test]
    fn answers_by_digit_and_checks_the_dialog() {
        let s = screen!("claude-permission-bash");
        let id = dialog(Harness::Claude, s).id;
        assert_eq!(
            answer(Harness::Claude, s, &id, "1").unwrap(),
            [Input::Keys(vec![Key::Char('1')])]
        );
        assert_eq!(
            answer(Harness::Claude, s, &id, CANCEL).unwrap(),
            [Input::Keys(vec![Key::Escape])]
        );
        assert_eq!(
            answer(Harness::Claude, s, &id, "9"),
            Err(DriverError::InvalidOption("9".into()))
        );
        // A different dialog is open now: refuse rather than answer it.
        let other = screen!("claude-permission-write");
        assert_eq!(
            answer(Harness::Claude, other, &id, "1"),
            Err(DriverError::UnknownRequest(id.clone()))
        );
        assert!(matches!(
            answer(Harness::Codex, s, &id, "1"),
            Err(DriverError::Unsupported { .. })
        ));
    }

    #[test]
    fn observer_reports_changes_only() {
        let mut o = ScreenObserver::new(Harness::Claude);
        let ev = o.feed(screen!("claude-working"), 1);
        assert!(matches!(&ev[..], [Event::StateChanged(_)]));
        assert!(o.feed(screen!("claude-working"), 2).is_empty());
        let ev = o.feed(screen!("claude-permission-bash"), 3);
        let Event::PermissionRequested {
            request_id,
            title,
            options,
        } = &ev[0]
        else {
            panic!("{ev:?}")
        };
        assert!(title.starts_with("Bash command · "), "{title}");
        assert_eq!(options.last().map(|o| o.id.as_str()), Some(CANCEL));
        assert_eq!(o.open_dialog(), Some(request_id.as_str()));
        assert!(matches!(ev[1], Event::StateChanged(_)));
        let id = request_id.clone();
        let ev = o.feed(screen!("claude-idle"), 4);
        assert_eq!(ev[0], Event::PermissionResolved { request_id: id });
        assert!(matches!(&ev[1], Event::StateChanged(s) if s.state == RunState::Idle));
        assert!(o.feed("garbage", 5).is_empty());
    }

    #[test]
    fn pi_without_osc7501() {
        for (screen, activity) in [
            (screen!("pi-0.82.1-working"), "Working..."),
            (screen!("pi-1.1.0-working"), "Working"),
        ] {
            let r = read(Harness::Pi, screen).unwrap();
            assert_eq!(r.state, RunState::Working { progress: None });
            assert_eq!(r.activity.as_deref(), Some(activity));
        }
        for screen in [screen!("pi-0.82.1-idle"), screen!("pi-1.1.0-idle")] {
            assert_eq!(read(Harness::Pi, screen).unwrap().state, RunState::Idle);
        }
    }
}
