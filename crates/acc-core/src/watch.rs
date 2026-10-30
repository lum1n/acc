//! Session events while no client is connected.
//!
//! Any client can be away: a phone in the background, a closed laptop, a
//! browser tab that was shut. [`watch_script`] starts a small follower on
//! the host, detached like the harness itself (nothing installed: `sh`,
//! `tail` and `awk`), that picks the moments worth telling someone about out
//! of the session log, using each harness's own markers: an approval or
//! question waiting, a turn done, an error, the harness exiting. Each is
//! one line of `events.ndjson` in the session directory, which a client
//! reads on reconnect to catch up ([`events_script`], or a fan-in
//! [`crate::detached::FeedPath::SessionEvents`] entry, then [`parse_events`]).
//! With a [`Hook`], the follower also runs an embedder's command or posts
//! to its URL for every event; how that becomes a notification is the
//! embedder's business. The follower ends when the harness does.

use serde::{Deserialize, Serialize};

use crate::detached::{EXIT_NOT_RUNNING, SessionDir};
use crate::harness::Harness;
use crate::shell::{LOGIN_PATH, quote};
use crate::state::BlockKind;

/// What the follower runs for every event, with the event's JSON line on
/// stdin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Hook {
    /// A shell command, run with `sh -c` on the host, with `ACC_SESSION`
    /// and `ACC_STATE` set.
    Command { command: String },
    /// Posted as `application/json` with `curl` (or `wget` when the host
    /// has no curl).
    Webhook { url: String },
}

/// What happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostState {
    /// The harness waits for an approval or an answer.
    Blocked,
    /// A turn ended.
    Done,
    Error,
    /// The harness process ended.
    Exited,
}

/// One line of `events.ndjson`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostEvent {
    /// Seconds since the Unix epoch, by the host's clock.
    pub at: u64,
    pub session: String,
    pub state: HostState,
    /// For [`HostState::Blocked`]: an approval or a question.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<BlockKind>,
    /// What it is about, when the log says cheaply: the tool, the command,
    /// the dialog's title. Plain characters only, at most 80.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// For [`HostState::Exited`]: the exit code, when the wrapper recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    /// The session log's length when it happened: replaying the log to here
    /// shows the session as it was.
    pub offset: u64,
}

/// The awk program that turns log lines into event lines. `h` is the
/// protocol (`claude`, `codex`, `pi` or `acp`), `sid` the session id, `off`
/// the log offset it starts at.
const PROGRAM: &str = r#"
function field(k,   m) {
  if (match($0, "\"" k "\": *\"[^\"\\\\]*\"")) {
    m = substr($0, RSTART, RLENGTH)
    sub(/^"[^"]*": *"/, "", m); sub(/"$/, "", m)
    gsub(/[^A-Za-z0-9 _.,:\/@=+-]/, "", m)
    return substr(m, 1, 80)
  }
  return ""
}
{
  off += length($0) + 1
  st = ""; kind = ""; det = ""
  if (h == "claude") {
    if ($0 ~ /"type": *"control_request"/ && $0 ~ /"subtype": *"can_use_tool"/) {
      st = "blocked"; det = field("tool_name")
      kind = (det == "AskUserQuestion") ? "question" : "permission"
    } else if ($0 ~ /"type": *"result"/) {
      st = ($0 ~ /"is_error": *true/) ? "error" : "done"
    }
  } else if (h == "codex") {
    if ($0 ~ /"method": *"item\/(commandExecution|fileChange|permissions)\/requestApproval"/) {
      st = "blocked"; kind = "permission"; det = field("command")
    } else if ($0 ~ /"method": *"(item\/tool\/requestUserInput|mcpServer\/elicitation\/request)"/) {
      st = "blocked"; kind = "question"
    } else if ($0 ~ /"method": *"turn\/completed"/) {
      root = field("rootTurnId")
      if (root == "" || root == field("id")) {
        if ($0 ~ /"status": *"failed", *"error"/) st = "error"
        else if ($0 !~ /"status": *"interrupted"/) st = "done"
      }
    } else if ($0 ~ /"method": *"error"/ && $0 !~ /"willRetry": *true/) {
      st = "error"
    }
  } else if (h == "pi") {
    if ($0 ~ /"type": *"extension_ui_request"/) {
      if ($0 ~ /"method": *"(select|confirm)"/) { st = "blocked"; kind = "permission"; det = field("title") }
      else if ($0 ~ /"method": *"(input|editor)"/) { st = "blocked"; kind = "question"; det = field("title") }
    } else if ($0 ~ /"type": *"message_end"/ && $0 ~ /"role": *"assistant"/) {
      if ($0 ~ /"stopReason": *"stop"/) st = "done"
      else if ($0 ~ /"stopReason": *"error"/) st = "error"
    }
  } else {
    if ($0 ~ /"method": *"session\/request_permission"/) {
      st = "blocked"; kind = "permission"; det = field("title")
    } else if ($0 ~ /"stopReason": *"/ && $0 !~ /"stopReason": *"cancelled"/) {
      st = "done"
    }
  }
  if (st != "") {
    srand(); now = srand()
    printf "{\"at\":%d,\"session\":\"%s\",\"state\":\"%s\"", now, sid, st
    if (kind != "") printf ",\"kind\":\"%s\"", kind
    if (det != "") printf ",\"detail\":\"%s\"", det
    printf ",\"offset\":%d}\n", off
    fflush()
  }
}
"#;

/// Script that starts the follower for session `dir` of `harness`,
/// replacing one already running there (to change its hook). Events start
/// from the log's current end. Exits [`EXIT_NOT_RUNNING`] when the session
/// has no running harness.
pub fn watch_script(dir: &SessionDir, harness: Harness, hook: Option<&Hook>) -> String {
    let id = dir.id();
    let hook = match hook {
        None => String::new(),
        Some(Hook::Command { command }) => format!(
            "st=${{1#*\\\"state\\\":\\\"}}; st=${{st%%\\\"*}}\n\
             printf '%s\\n' \"$1\" | ACC_SESSION={id} ACC_STATE=\"$st\" sh -c {} >/dev/null 2>&1 &\n",
            quote(command)
        ),
        Some(Hook::Webhook { url }) => format!(
            "if command -v curl >/dev/null 2>&1; then\n\
             printf '%s\\n' \"$1\" | curl -fsS -m 10 -X POST -H 'Content-Type: application/json' --data-binary @- {url} >/dev/null 2>&1 &\n\
             else\n\
             wget -q -T 10 -O /dev/null --header='Content-Type: application/json' --post-data=\"$1\" {url} >/dev/null 2>&1 &\n\
             fi\n",
            url = quote(url)
        ),
    };
    // `$1` is the session directory. The follower records its PID, follows
    // the log through awk until the harness is gone, lets it drain, then
    // stops the tail (so every line is handled) and records the exit.
    let body = format!(
        "cd \"$1\" || exit 1\n\
         echo $$ >watch.pid\n\
         pid=$(cat pid 2>/dev/null)\n\
         LC_ALL=C; export LC_ALL\n\
         emit() {{\n\
         printf '%s\\n' \"$1\" >>events.ndjson\n\
         {hook}\
         }}\n\
         s=$(wc -c <out.ndjson | tr -d ' ')\n\
         {{ tail -c +$((s + 1)) -f out.ndjson & echo $! >watch.tail; wait; }} \
         | awk -v h={harness} -v sid={id} -v off=\"$s\" {program} \
         | while IFS= read -r e; do emit \"$e\"; done &\n\
         w=$!\n\
         while kill -0 \"$pid\" 2>/dev/null && [ ! -f exit ]; do sleep 1; done\n\
         sleep 1\n\
         kill \"$(cat watch.tail)\" 2>/dev/null\n\
         wait $w\n\
         c=$(cat exit 2>/dev/null); case \"$c\" in ''|*[!0-9-]*) c=null;; esac\n\
         emit \"{{\\\"at\\\":$(date +%s),\\\"session\\\":\\\"{id}\\\",\\\"state\\\":\\\"exited\\\",\\\"code\\\":$c,\\\"offset\\\":$(wc -c <out.ndjson | tr -d ' ')}}\"\n\
         rm -f watch.pid watch.tail\n",
        harness = match harness {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
            Harness::Pi => "pi",
            Harness::Cursor | Harness::Copilot | Harness::Opencode => "acp",
        },
        program = quote(PROGRAM),
    );
    let d = dir.dir_expr();
    format!(
        "{LOGIN_PATH}\
         d={d}\n\
         pid=$(cat \"$d/pid\" 2>/dev/null) && kill -0 \"$pid\" 2>/dev/null && [ ! -f \"$d/exit\" ] || exit {EXIT_NOT_RUNNING}\n\
         if [ -f \"$d/watch.pid\" ]; then\n\
         old=$(cat \"$d/watch.pid\")\n\
         kill \"$(cat \"$d/watch.tail\" 2>/dev/null)\" \"$old\" 2>/dev/null\n\
         rm -f \"$d/watch.pid\" \"$d/watch.tail\"\n\
         fi\n\
         nohup sh -c {body} acc-watch \"$d\" </dev/null >/dev/null 2>&1 &\n",
        body = quote(&body),
    )
}

/// Script printing `events.ndjson` from byte `offset`.
pub fn events_script(dir: &SessionDir, offset: u64) -> String {
    format!(
        "f={}\n[ -f \"$f\" ] && tail -c +{} \"$f\"\ntrue\n",
        dir.events_path(),
        offset + 1
    )
}

/// Parses `events.ndjson` lines, skipping any it can't read.
pub fn parse_events(text: &str) -> Vec<HostEvent> {
    text.lines()
        .filter_map(|l| serde_json::from_str(l.trim()).ok())
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn hooks_are_quoted_into_the_follower() {
        let dir = SessionDir::new("s1").unwrap();
        let s = watch_script(
            &dir,
            Harness::Claude,
            Some(&Hook::Command {
                command: "notify-send 'it''s done'".into(),
            }),
        );
        assert!(s.contains("nohup sh -c") && s.contains("h=claude"), "{s}");
        let s = watch_script(
            &dir,
            Harness::Codex,
            Some(&Hook::Webhook {
                url: "https://example.com/hook?a=1&b=2".into(),
            }),
        );
        assert!(s.contains("curl") && s.contains("wget"));
        assert!(events_script(&dir, 10).contains("tail -c +11"));
    }

    #[test]
    fn parses_events_and_skips_junk() {
        let got = parse_events(
            "{\"at\":1,\"session\":\"s1\",\"state\":\"blocked\",\"kind\":\"permission\",\"detail\":\"Bash\",\"offset\":9}\n\
             nope\n\
             {\"at\":2,\"session\":\"s1\",\"state\":\"exited\",\"code\":null,\"offset\":12}\n",
        );
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].kind, Some(BlockKind::Permission));
        assert_eq!(got[1].state, HostState::Exited);
        assert_eq!(got[1].code, None);
    }
}

/// The follower against a real `sh`, `tail` and `awk`, on recorded logs.
#[cfg(all(test, unix))]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod sh_tests {
    use super::*;
    use crate::detached::ROOT;
    use crate::harness::{LaunchSpec, Protocol};
    use std::io::Write as _;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn sh(home: &Path, script: &str) -> (String, Option<i32>) {
        let out = Command::new("sh")
            .arg("-c")
            .arg(script)
            .env("HOME", home)
            .stdin(Stdio::null())
            .output()
            .expect("sh");
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            out.status.code(),
        )
    }

    fn wait_for(path: &Path, what: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut content = String::new();
        while Instant::now() < deadline {
            content = std::fs::read_to_string(path).unwrap_or_default();
            if what(&content) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        content
    }

    /// Runs `cat` as the harness, starts the follower, writes `log` through
    /// the FIFO, stops the harness, and returns the events.
    fn follow(
        tag: &str,
        harness: Harness,
        log: &str,
        hook: Option<Hook>,
    ) -> (Vec<HostEvent>, std::path::PathBuf) {
        let home = std::env::temp_dir().join(format!("acc-watch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let dir = SessionDir::new("w1").unwrap();
        let spec = LaunchSpec {
            program: "cat".into(),
            args: vec![],
            env: vec![],
            protocol: Protocol::Acp,
            sandbox: None,
        };
        sh(&home, &dir.launch_script(&spec, "/"));
        assert_eq!(sh(&home, &watch_script(&dir, harness, hook.as_ref())).1, Some(0));
        let d = home.join(ROOT).join("w1");
        wait_for(&d.join("watch.tail"), |c| !c.is_empty());
        let mut input = Command::new("sh")
            .arg("-c")
            .arg(dir.input_script())
            .env("HOME", &home)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        input.stdin.take().unwrap().write_all(log.as_bytes()).unwrap();
        input.wait().unwrap();
        wait_for(&d.join("out.ndjson"), |c| c.len() >= log.len());
        sh(&home, &dir.stop_script());
        let events = wait_for(&d.join("events.ndjson"), |c| c.contains("\"exited\""));
        (parse_events(&events), home)
    }

    #[test]
    fn claude_log_gives_blocked_done_and_exited_and_runs_the_hook() {
        let log = include_str!("../tests/fixtures/claude/permission-deny.ndjson");
        let (events, home) = follow(
            "claude",
            Harness::Claude,
            log,
            Some(Hook::Command {
                command: "cat >>\"$HOME/hook.out\"; echo \"$ACC_STATE\" >>\"$HOME/hook.out\"".into(),
            }),
        );
        let states: Vec<_> = events.iter().map(|e| (e.state, e.kind)).collect();
        assert_eq!(
            states,
            [
                (HostState::Blocked, Some(BlockKind::Permission)),
                (HostState::Done, None),
                (HostState::Exited, None),
            ],
            "{events:?}"
        );
        assert_eq!(events[0].detail.as_deref(), Some("Bash"));
        assert_eq!(events[0].session, "w1");
        assert!(events[0].offset > 0 && events[0].offset < events[1].offset);
        let result = log.find("\"type\":\"result\"").unwrap();
        let end = result + log[result..].find('\n').unwrap() + 1;
        assert_eq!(
            events[1].offset as usize, end,
            "the offset is just past the result line"
        );
        let hook = wait_for(&home.join("hook.out"), |c| c.contains("exited"));
        assert!(
            hook.contains("\"state\":\"blocked\"") && hook.contains("\ndone\n"),
            "{hook}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn other_harnesses_mark_their_turns() {
        let codex = include_str!("../tests/fixtures/codex/subagent-0.162.1.ndjson");
        let (events, home) = follow("codex", Harness::Codex, codex, None);
        let done = events.iter().filter(|e| e.state == HostState::Done).count();
        assert_eq!(done, 1, "the subagent's own turn isn't the session's: {events:?}");
        let _ = std::fs::remove_dir_all(&home);

        let cursor = include_str!("../tests/fixtures/acp/cursor-2026.10.01.out.ndjson");
        let (events, home) = follow("cursor", Harness::Cursor, cursor, None);
        let states: Vec<_> = events.iter().map(|e| e.state).collect();
        assert_eq!(
            states,
            [HostState::Blocked, HostState::Done, HostState::Exited],
            "{events:?}"
        );
        let _ = std::fs::remove_dir_all(&home);

        let pi = include_str!("../tests/fixtures/pi/rpc-turn-with-dialog.jsonl");
        let (events, home) = follow("pi", Harness::Pi, pi, None);
        assert!(events.iter().any(|e| e.state == HostState::Blocked), "{events:?}");
        assert!(events.iter().any(|e| e.state == HostState::Done), "{events:?}");
        let _ = std::fs::remove_dir_all(&home);
    }
}
