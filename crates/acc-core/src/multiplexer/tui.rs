//! Co-driving a harness TUI in a pane: ACC commands as keystrokes.
//!
//! When a harness runs interactively in a multiplexer, ACC types into its
//! pane like the user would. This is per harness, and only covers what each
//! TUI does reliably from its prompt: submitting text, interrupting, built-in
//! slash commands and exiting. Answering approval dialogs needs the rendered
//! screen: see [`super::screen_match`].
//!
//! pi is covered fully (checked against pi 1.1.0's keybindings and slash
//! commands). Other harnesses get prompts and the commands whose keys are
//! well known; the rest return [`DriverError::Unsupported`].

use crate::command::{Command, OptionId};
use crate::driver::DriverError;
use crate::harness::Harness;

use super::{Input, Key, Program};

/// Inputs that submit `text` at the TUI's prompt: typed when it is one line,
/// a bracketed paste otherwise, then Enter.
pub fn submit(text: &str) -> Vec<Input> {
    let body = if text.contains(['\n', '\r']) {
        Input::Paste(text.to_owned())
    } else {
        Input::Text(text.to_owned())
    };
    vec![body, Input::Keys(vec![Key::Enter])]
}

fn unsupported(harness: Harness, what: &'static str) -> DriverError {
    DriverError::Unsupported { harness, what }
}

/// Keystrokes that carry out `cmd` in `harness`'s TUI.
pub fn inputs(harness: Harness, cmd: &Command) -> Result<Vec<Input>, DriverError> {
    let slash = |s: String| Ok(submit(&s));
    let with_arg = |name: &str, arg: &str| {
        if arg.is_empty() {
            format!("/{name}")
        } else {
            format!("/{name} {arg}")
        }
    };
    match (harness, cmd) {
        (_, Command::Prompt { text, attachments }) if attachments.is_empty() => Ok(submit(text)),
        // Images only travel over the headless protocols.
        (h, Command::Prompt { .. }) => Err(unsupported(h, "images from a pane")),
        (_, Command::Invoke { name, args }) => slash(with_arg(name.trim_start_matches('/'), args)),
        (Harness::Pi | Harness::Claude | Harness::Codex, Command::Interrupt) => {
            Ok(vec![Input::Keys(vec![Key::Escape])])
        }
        (Harness::Pi | Harness::Claude | Harness::Codex, Command::Compact { hint }) => {
            slash(with_arg("compact", hint.as_deref().unwrap_or("")))
        }
        (Harness::Pi | Harness::Codex, Command::New) => slash("/new".into()),
        (Harness::Claude, Command::New) => slash("/clear".into()),
        (Harness::Pi, Command::Fork) => slash("/clone".into()),
        (Harness::Pi, Command::Rename { name }) => slash(with_arg("name", name)),
        (Harness::Pi | Harness::Claude, Command::Shell { command }) => slash(format!("!{command}")),
        (
            Harness::Pi,
            Command::SetOption {
                id: OptionId::Model,
                value,
            },
        ) => slash(with_arg("model", value)),
        (
            Harness::Pi,
            Command::SetOption {
                id: OptionId::Effort,
                value,
            },
        ) => slash(with_arg("thinking", value)),
        (
            Harness::Claude,
            Command::SetOption {
                id: OptionId::Model,
                value,
            },
        ) => slash(with_arg("model", value)),
        // Picking an option depends on the dialog on screen right now.
        (_, Command::AnswerPermission { .. }) => Err(unsupported(
            harness,
            "answering dialogs without the screen (use screen_match::answer)",
        )),
        (h, Command::Interrupt) => Err(unsupported(h, "interrupt from a pane")),
        (h, _) => Err(unsupported(h, "this command from a pane")),
    }
}

/// Keystrokes that close the TUI cleanly (for takeover: close it, then
/// resume the session headless). Interrupt a running turn first.
pub fn exit_inputs(harness: Harness) -> Result<Vec<Input>, DriverError> {
    match harness {
        Harness::Pi | Harness::Codex => Ok(submit("/quit")),
        Harness::Claude => Ok(submit("/exit")),
        h => Err(unsupported(h, "a known exit command")),
    }
}

/// The interactive TUI for `harness`, resuming `resume` when given. pi gets
/// `PI_PROGRAM_STATUS=1`, so it reports OSC 7501 through tmux and screen,
/// which never answer the probe.
pub fn launch(harness: Harness, resume: Option<&str>) -> Result<Program, DriverError> {
    let mut program = Program {
        program: harness.program().to_owned(),
        args: Vec::new(),
        env: Vec::new(),
    };
    match (harness, resume) {
        (Harness::Pi, r) => {
            program.env.push(("PI_PROGRAM_STATUS".into(), "1".into()));
            if let Some(path) = r {
                program.args = vec!["--session".into(), path.into()];
            }
        }
        (Harness::Claude, Some(id)) => program.args = vec!["--resume".into(), id.into()],
        (Harness::Codex, Some(id)) => program.args = vec!["resume".into(), id.into()],
        (_, None) => {}
        (h, Some(_)) => return Err(unsupported(h, "resuming a TUI session by id")),
    }
    Ok(program)
}

/// Keystrokes that hand a session back to its TUI in the user's own pane,
/// after ACC closed the TUI there: types the resume command into the shell
/// left in the pane.
pub fn resume_inputs(harness: Harness, resume: &str) -> Result<Vec<Input>, DriverError> {
    let argv = resume_argv(harness, resume, None)?;
    Ok(submit(&crate::shell::join(argv.iter().map(String::as_str))))
}

/// The TUI command resuming `resume`, run the way `launcher` describes
/// (inside the user's sandbox, with their executable and flags).
pub fn resume_argv(
    harness: Harness,
    resume: &str,
    launcher: Option<&crate::harness::Launcher>,
) -> Result<Vec<String>, DriverError> {
    let mut program = launch(harness, Some(resume))?;
    let Some(l) = launcher else {
        return Ok(program.argv());
    };
    if let Some(p) = &l.program {
        program.program = p.clone();
    }
    program.args.extend(l.args.iter().cloned());
    let mut argv = l.prefix.clone();
    argv.extend(program.argv());
    Ok(argv)
}

/// Whether a captured pane shows `harness`'s approval dialog, which its
/// transcript doesn't record. Only the bottom of the screen counts: the
/// dialog replaces the prompt there, while the same words higher up are
/// conversation.
pub fn shows_approval(harness: Harness, screen: &str) -> bool {
    match harness {
        // "Do you want to proceed?" (or "make this edit to", "allow Claude
        // to fetch", ...) over numbered choices starting with "1. Yes".
        Harness::Claude => {
            let tail: Vec<&str> = screen
                .lines()
                .map(|l| l.trim_matches(|c: char| c.is_whitespace() || "│|❯>›".contains(c)))
                .filter(|l| !l.is_empty())
                .collect();
            let tail = &tail[tail.len().saturating_sub(25)..];
            let question = tail.iter().position(|l| l.starts_with("Do you want to"));
            question.is_some_and(|q| tail[q..].iter().any(|l| l.starts_with("1. Yes")))
        }
        _ => false,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn prompts_type_or_paste() {
        assert_eq!(
            inputs(Harness::Cursor, &Command::prompt("hi")).unwrap(),
            [Input::Text("hi".into()), Input::Keys(vec![Key::Enter])]
        );
        assert!(matches!(
            inputs(Harness::Pi, &Command::prompt("a\nb")).unwrap()[0],
            Input::Paste(_)
        ));
        // A pane takes keystrokes, never an image.
        let with_image = Command::Prompt {
            text: "look".into(),
            attachments: vec![crate::command::Attachment::from_bytes("image/png", b"PNG")],
        };
        assert!(matches!(
            inputs(Harness::Pi, &with_image),
            Err(DriverError::Unsupported { .. })
        ));
    }

    #[test]
    fn pi_commands_are_slash_commands() {
        let first = |c: Command| inputs(Harness::Pi, &c).unwrap().remove(0);
        assert_eq!(
            first(Command::Compact { hint: None }),
            Input::Text("/compact".into())
        );
        assert_eq!(
            first(Command::Rename { name: "x y".into() }),
            Input::Text("/name x y".into())
        );
        assert_eq!(first(Command::Fork), Input::Text("/clone".into()));
        assert_eq!(
            first(Command::Shell { command: "ls".into() }),
            Input::Text("!ls".into())
        );
        assert_eq!(first(Command::Interrupt), Input::Keys(vec![Key::Escape]));
        assert!(inputs(Harness::Pi, &Command::Review).is_err());
    }

    #[test]
    fn launch_and_hand_back() {
        let p = launch(Harness::Pi, Some("/s/x.jsonl")).unwrap();
        assert_eq!(
            p.argv(),
            ["env", "PI_PROGRAM_STATUS=1", "pi", "--session", "/s/x.jsonl"]
        );
        assert_eq!(
            resume_inputs(Harness::Claude, "abc").unwrap()[0],
            Input::Text("claude --resume abc".into())
        );
        assert!(launch(Harness::Cursor, Some("x")).is_err());
        assert_eq!(exit_inputs(Harness::Pi).unwrap()[0], Input::Text("/quit".into()));
    }

    #[test]
    fn claude_approval_dialog_on_screen() {
        let dialog = "● Running the build\n\n Bash command\n\n   cargo build\n   Build it\n\n \
            Do you want to proceed?\n ❯ 1. Yes\n   2. Yes, and don't ask again for cargo build commands\n   \
            3. No, and tell Claude what to do differently (esc)\n\n";
        assert!(shows_approval(Harness::Claude, dialog));
        let boxed = "│ Do you want to make this edit to main.rs?\n│ > 1. Yes\n│   2. No\n";
        assert!(shows_approval(Harness::Claude, boxed));
        let working = "● Bash(cargo build)\n  ⎿  Running…\n\n✻ Compiling… (12s · esc to interrupt)\n\n> \n  ? for shortcuts\n";
        assert!(!shows_approval(Harness::Claude, working));
        let quoted = "> Do you want to proceed? is what it asks\n● Sure.\n> \n";
        assert!(!shows_approval(Harness::Claude, quoted));
        assert!(!shows_approval(Harness::Pi, dialog));
    }
}
