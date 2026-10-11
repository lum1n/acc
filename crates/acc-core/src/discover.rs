//! Finding harnesses ACC didn't start: TUIs a user runs in a terminal or a
//! multiplexer pane.
//!
//! One host script lists harness processes with their working directory,
//! multiplexer environment, open transcript files, and the newest transcript
//! where the harness keeps them, plus each tmux pane's session and window.
//! [`parse_discover`] turns that into [`ForeignAgent`]s a client can observe
//! (tail the transcript) and take over. Nothing is installed on the host.

use serde::{Deserialize, Serialize};

use crate::harness::Harness;
use crate::multiplexer::{self, PaneRef};

/// A transcript file a foreign agent writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptRef {
    /// Absolute path on the host.
    pub path: String,
    /// Bytes in the file when discovered; earlier content is history.
    pub size: u64,
    /// True when the process has the file open (certain), false when it is
    /// the newest file in the harness's directory (a good guess).
    pub exact: bool,
}

/// A harness process ACC didn't start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignAgent {
    pub harness: Harness,
    pub pid: u32,
    pub args: String,
    pub cwd: Option<String>,
    /// Seconds since the process started.
    pub elapsed_secs: Option<u64>,
    /// The multiplexer pane it runs in, when it runs in one.
    pub pane: Option<PaneRef>,
    /// `session:window` of a tmux pane, for display.
    pub pane_label: Option<String>,
    pub transcript: Option<TranscriptRef>,
    /// The harness's session id, when the transcript or arguments tell.
    pub native_id: Option<String>,
}

impl ForeignAgent {
    /// A stable key for this agent while its process lives.
    pub fn key(&self) -> String {
        format!("tui-{}-{}", self.harness.program(), self.pid)
    }
}

/// Script listing foreign harness processes. Parse with [`parse_discover`].
pub fn discover_script() -> String {
    r#"e() { printf '%s' "$1" | sed 's/[^A-Za-z0-9-]/-/g'; }
ps -eo pid=,etime=,args= 2>/dev/null | while read -r pid etime args; do
  # Split the command line into words without globbing them.
  set -f; set -- $args; set +f
  b=${1##*/}
  case $b in node|bun|deno|python*) b=${2##*/} ;; esac
  case $b in claude|codex|agent|cursor-agent|copilot|pi|opencode) ;; *) continue ;; esac
  printf 'A\t%s\t%s\t%s\t%s\n' "$pid" "$b" "$etime" "$args"
  cwd=$(readlink "/proc/$pid/cwd" 2>/dev/null || lsof -a -p "$pid" -d cwd -Fn 2>/dev/null | sed -n 's/^n//p' | head -n 1)
  printf 'C\t%s\t%s\n' "$pid" "$cwd"
  if [ -r "/proc/$pid/environ" ]; then
    tr '\0' '\n' <"/proc/$pid/environ" 2>/dev/null
  else
    ps eww -o command= -p "$pid" 2>/dev/null | tr ' ' '\n'
  fi | grep -E '^(TMUX|TMUX_PANE|ZELLIJ_SESSION_NAME|ZELLIJ_PANE_ID|STY|WINDOW)=' | while read -r kv; do
    printf 'E\t%s\t%s\n' "$pid" "$kv"
  done
  if [ -d "/proc/$pid/fd" ]; then
    for f in /proc/$pid/fd/*; do readlink "$f" 2>/dev/null; done
  else
    lsof -p "$pid" -Fn 2>/dev/null | sed -n 's/^n//p'
  fi | grep '\.jsonl$' | sort -u | while read -r t; do
    printf 'F\t%s\t%s\t%s\n' "$pid" "$t" "$(wc -c <"$t" 2>/dev/null | tr -d ' ')"
  done
  d=
  case $b in
    claude) d="$HOME/.claude/projects/$(e "$cwd")" ;;
    pi) d="$HOME/.pi/agent/sessions/--$(printf '%s' "${cwd#/}" | tr '/:' '--')--" ;;
  esac
  n=
  if [ -n "$d" ]; then
    n=$(ls -t "$d"/*.jsonl 2>/dev/null | head -n 1)
  elif [ "$b" = codex ]; then
    n=$(find "$HOME/.codex/sessions" -type f -name 'rollout-*.jsonl' -mtime -2 2>/dev/null | while read -r f; do printf '%s\t%s\n' "$(stat -c %Y "$f" 2>/dev/null || stat -f %m "$f")" "$f"; done | sort -rn | head -n 1 | cut -f2-)
  fi
  [ -n "$n" ] && printf 'N\t%s\t%s\t%s\n' "$pid" "$n" "$(wc -c <"$n" 2>/dev/null | tr -d ' ')"
done
for s in "" $(for s in "${TMUX_TMPDIR:-/tmp}/tmux-$(id -u)"/*; do [ -S "$s" ] && printf '%s ' "$s"; done); do
  if [ -n "$s" ]; then t="tmux -S $s"; else t=tmux; fi
  $t list-panes -a -F 'P	#{pane_id}	#{session_name}:#{window_index}	#{socket_path}' 2>/dev/null
done
true
"#
    .to_owned()
}

/// Seconds from `ps` `etime` (`[[dd-]hh:]mm:ss`).
fn parse_etime(s: &str) -> Option<u64> {
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<u64>().ok()?, r),
        None => (0, s),
    };
    let mut secs = 0u64;
    for part in rest.split(':') {
        secs = secs * 60 + part.parse::<u64>().ok()?;
    }
    Some(days * 86_400 + secs)
}

/// Which harness an argv runs interactively. Headless modes (ACC's own
/// detached sessions, scripts, servers) are not foreign TUIs.
pub fn classify(program: &str, args: &str) -> Option<Harness> {
    let harness = match program {
        "claude" => Harness::Claude,
        "codex" => Harness::Codex,
        "agent" | "cursor-agent" => Harness::Cursor,
        "copilot" => Harness::Copilot,
        "pi" => Harness::Pi,
        "opencode" => Harness::Opencode,
        _ => return None,
    };
    let words: Vec<&str> = args.split_whitespace().collect();
    let has = |w: &str| words.contains(&w);
    let headless = match harness {
        Harness::Claude => has("-p") || has("--print") || has("stream-json"),
        Harness::Codex => has("app-server") || has("exec") || has("mcp") || has("mcp-server") || has("proto"),
        Harness::Cursor => has("acp") || has("-p") || has("--print"),
        Harness::Copilot => has("--acp") || has("-p") || has("--prompt"),
        Harness::Pi => words
            .windows(2)
            .any(|w| w == ["--mode", "rpc"] || w == ["--mode", "json"]),
        Harness::Opencode => has("acp") || has("serve") || has("run"),
    };
    (!headless).then_some(harness)
}

/// Parses the output of [`discover_script`].
pub fn parse_discover(out: &str) -> Vec<ForeignAgent> {
    use std::collections::HashMap;
    let mut agents: Vec<ForeignAgent> = Vec::new();
    let mut env: HashMap<u32, Vec<(String, String)>> = HashMap::new();
    let mut open: HashMap<u32, Vec<TranscriptRef>> = HashMap::new();
    let mut newest: HashMap<u32, TranscriptRef> = HashMap::new();
    let mut labels: HashMap<String, String> = HashMap::new();
    for line in out.lines() {
        let f: Vec<&str> = line.splitn(5, '\t').collect();
        let pid = || f.get(1).and_then(|p| p.trim().parse::<u32>().ok());
        let tref = |exact| {
            Some(TranscriptRef {
                path: (*f.get(2)?).to_owned(),
                size: f.get(3).and_then(|s| s.trim().parse().ok()).unwrap_or(0),
                exact,
            })
        };
        match f.first().copied() {
            Some("A") if f.len() == 5 => {
                let (Some(pid), Some(harness)) = (pid(), classify(f[2], f[4])) else {
                    continue;
                };
                agents.push(ForeignAgent {
                    harness,
                    pid,
                    args: f[4].to_owned(),
                    cwd: None,
                    elapsed_secs: parse_etime(f[3].trim()),
                    pane: None,
                    pane_label: None,
                    transcript: None,
                    native_id: None,
                });
            }
            Some("C") => {
                if let (Some(pid), Some(cwd)) = (pid(), f.get(2).filter(|c| !c.is_empty()))
                    && let Some(a) = agents.iter_mut().find(|a| a.pid == pid)
                {
                    a.cwd = Some((*cwd).to_owned());
                }
            }
            Some("E") => {
                if let (Some(pid), Some((k, v))) = (pid(), f.get(2).and_then(|kv| kv.split_once('='))) {
                    env.entry(pid).or_default().push((k.to_owned(), v.to_owned()));
                }
            }
            Some("F") => {
                if let (Some(pid), Some(t)) = (pid(), tref(true)) {
                    open.entry(pid).or_default().push(t);
                }
            }
            Some("N") => {
                if let (Some(pid), Some(t)) = (pid(), tref(false)) {
                    newest.insert(pid, t);
                }
            }
            Some("P") if f.len() >= 3 => {
                labels.insert(f[1].to_owned(), f[2].to_owned());
            }
            _ => {}
        }
    }
    for a in &mut agents {
        if let Some(vars) = env.get(&a.pid) {
            a.pane = multiplexer::locate(vars.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            if let Some(p) = &a.pane {
                a.pane_label = labels.get(&p.pane).cloned();
            }
        }
        // An open file is certain; for Claude prefer a session file over
        // other JSONL it may hold open (history, todos).
        let opened = open.remove(&a.pid).unwrap_or_default();
        let pick = opened
            .iter()
            .find(|t| match a.harness {
                Harness::Claude => t.path.contains("/.claude/projects/"),
                Harness::Codex => t.path.contains("/rollout-"),
                Harness::Pi => t.path.contains("/.pi/agent/sessions/"),
                _ => true,
            })
            .cloned();
        a.transcript = pick.or_else(|| newest.remove(&a.pid));
        a.native_id = native_id(a);
    }
    agents
}

/// The harness session id: from the transcript the process has open, else
/// its arguments (`--resume X`, `codex resume X`), else the guessed file.
fn native_id(a: &ForeignAgent) -> Option<String> {
    let from_file = || from_path(a.harness, &a.transcript.as_ref()?.path);
    if a.transcript.as_ref().is_some_and(|t| t.exact)
        && let Some(id) = from_file()
    {
        return Some(id);
    }
    let words: Vec<&str> = a.args.split_whitespace().collect();
    for flag in ["--resume", "-r", "--session-id", "--session", "resume"] {
        if let Some(i) = words.iter().position(|w| *w == flag)
            && let Some(v) = words.get(i + 1)
            && !v.starts_with('-')
        {
            return Some((*v).to_owned());
        }
    }
    from_file()
}

fn from_path(harness: Harness, path: &str) -> Option<String> {
    match harness {
        // pi resumes by session file path.
        Harness::Pi => Some(path.to_owned()),
        Harness::Claude => path
            .rsplit('/')
            .next()
            .and_then(|f| f.strip_suffix(".jsonl"))
            .map(str::to_owned),
        Harness::Codex => crate::codex::rollout::thread_id_from_path(path).map(str::to_owned),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::multiplexer::Multiplexer;

    const OUT: &str = "A\t4242\tclaude\t01:02:03\tnode /usr/lib/node_modules/claude/cli.js --model opus\n\
C\t4242\t/home/v/proj\n\
E\t4242\tTMUX=/tmp/tmux-1000/default,999,0\n\
E\t4242\tTMUX_PANE=%3\n\
F\t4242\t/home/v/.claude/todos/x.jsonl\t10\n\
N\t4242\t/home/v/.claude/projects/-home-v-proj/5f3c1a52-8a3e-4c1e-9d47-2b6f0e9a7c11.jsonl\t2048\n\
A\t77\tclaude\t00:05\tclaude -p --output-format stream-json\n\
A\t88\tcodex\t1-00:00:10\tcodex resume abc\n\
C\t88\t/srv/app\n\
F\t88\t/home/v/.codex/sessions/2026/10/10/rollout-2026-10-10T12-13-12-01a125bb-467b-7971-9643-70534831e6ee.jsonl\t900\n\
P\t%3\twork:2\t/tmp/tmux-1000/default\n";

    #[test]
    fn finds_tuis_with_pane_and_transcript() {
        let found = parse_discover(OUT);
        assert_eq!(found.len(), 2, "headless claude -p is skipped");
        let c = &found[0];
        assert_eq!(
            (c.harness, c.pid, c.elapsed_secs),
            (Harness::Claude, 4242, Some(3723))
        );
        assert_eq!(c.cwd.as_deref(), Some("/home/v/proj"));
        let pane = c.pane.as_ref().unwrap();
        assert_eq!((pane.multiplexer, pane.pane.as_str()), (Multiplexer::Tmux, "%3"));
        assert_eq!(c.pane_label.as_deref(), Some("work:2"));
        let t = c.transcript.as_ref().unwrap();
        assert!(!t.exact && t.size == 2048 && t.path.contains("projects"));
        assert_eq!(
            c.native_id.as_deref(),
            Some("5f3c1a52-8a3e-4c1e-9d47-2b6f0e9a7c11")
        );

        let x = &found[1];
        assert_eq!((x.harness, x.elapsed_secs), (Harness::Codex, Some(86_410)));
        assert!(x.transcript.as_ref().unwrap().exact);
        assert_eq!(
            x.native_id.as_deref(),
            Some("01a125bb-467b-7971-9643-70534831e6ee"),
            "the open rollout is certain"
        );
        assert!(x.pane.is_none());
    }

    #[test]
    fn classify_skips_headless_modes() {
        assert_eq!(classify("codex", "codex"), Some(Harness::Codex));
        assert_eq!(classify("codex", "codex app-server"), None);
        assert_eq!(classify("pi", "pi --mode rpc"), None);
        assert_eq!(classify("agent", "agent acp"), None);
        assert_eq!(classify("agent", "agent"), Some(Harness::Cursor));
        assert_eq!(classify("vim", "vim"), None);
    }

    #[cfg(unix)]
    #[test]
    fn script_runs_under_sh() {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(discover_script())
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        // Whatever runs here, parsing must not panic.
        let _ = parse_discover(&String::from_utf8_lossy(&out.stdout));
    }
}
