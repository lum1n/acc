//! Daemonless, disconnect-proof sessions.
//!
//! The harness runs detached on the host with its stdin on a FIFO and its
//! stdout appended to a log. A client (desktop, or a phone over SSH) attaches
//! by tailing the log from a byte offset and writing to the FIFO. Only POSIX
//! tools (`sh`, `mkdir`, `mkfifo`, `nohup`, `tail`, `cat`, `wc`, `grep`) are
//! used, so nothing is installed on the host. See
//! `research/daemonless-ssh-control.md` §2 and §4.
//!
//! Layout under `$HOME/.acc/s/<session>/`:
//! `in` (FIFO), `out.ndjson`, `err`, `pid`, `exit`, `spec.json` (one line, a
//! [`crate::session::SessionSpec`]), `sent` (ids of writes already
//! delivered), `lock` (held while writing), and `events.ndjson`,
//! `watch.pid` and `watch.tail` while a [`crate::watch`] follower runs.
//!
//! These functions only build scripts and parse their output; running them
//! (locally, or over an SSH exec channel with [`crate::shell::ssh_command`])
//! is up to the transport.

use crate::harness::LaunchSpec;
use crate::shell::{LOGIN_PATH, join, quote};

/// Root directory for detached sessions, relative to `$HOME`.
pub const ROOT: &str = ".acc/s";

/// Exit code of [`SessionDir::attach_script`], [`SessionDir::input_script`],
/// [`SessionDir::write_script`] and [`SessionDir::stop_script`] when the
/// session has no running harness.
pub const EXIT_NOT_RUNNING: u32 = 3;

/// Exit code of [`SessionDir::write_script`] when fewer bytes arrived than
/// announced (the connection dropped mid-upload). Nothing was written.
pub const EXIT_INCOMPLETE: u32 = 4;
/// Launch exit status when the harness could not start in its working
/// directory (it does not exist, or is not enterable).
pub const EXIT_NO_CWD: u32 = 5;
/// How many 20 ms ticks the launch script waits for the harness PID before
/// giving up, so a wrapper that dies without a trace cannot hang the launch.
pub const LAUNCH_WAIT_TICKS: u32 = 500;

/// A detached session's directory on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDir {
    id: String,
}

impl SessionDir {
    /// `id` must be ACC-generated: `[A-Za-z0-9_-]{1,64}`.
    pub fn new(id: &str) -> Option<Self> {
        let ok = (1..=64).contains(&id.len())
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        ok.then(|| Self { id: id.to_owned() })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Shell expression for the directory (expands `$HOME` on the host).
    fn dir(&self) -> String {
        format!("\"$HOME\"/{ROOT}/{}", self.id)
    }

    /// Script that creates the session and starts the harness detached.
    ///
    /// The harness is found on the `PATH` of the user's login shell. The FIFO
    /// is opened read-write (`<>`) so the harness never sees EOF when a client
    /// disconnects, and `nohup` keeps it alive after SSH hangs up. Prints the
    /// harness PID on success. Relaunching an existing id (to resume) keeps
    /// appending to the same log, so client offsets stay valid.
    pub fn launch_script(&self, spec: &LaunchSpec, cwd: &str) -> String {
        self.launch_script_with_spec(spec, cwd, None)
    }

    /// [`SessionDir::launch_script`] that also records `spec_json` (one
    /// line, a [`crate::session::SessionSpec`]) as `spec.json`, so other
    /// clients can list and attach.
    pub fn launch_script_with_spec(&self, spec: &LaunchSpec, cwd: &str, spec_json: Option<&str>) -> String {
        let env = spec
            .env
            .iter()
            .map(|(k, v)| format!("{k}={}", quote(v)))
            .collect::<Vec<_>>()
            .join(" ");
        let cmd = join(std::iter::once(spec.program.as_str()).chain(spec.args.iter().map(String::as_str)));
        let env = if env.is_empty() {
            String::new()
        } else {
            format!("env {env} ")
        };
        // The wrapper starts the harness, records its PID, waits for it and
        // records the exit code. `$0` is the session directory. A `cd` that
        // fails is recorded as an exit too, so the launcher below sees it
        // instead of waiting for a PID that never comes.
        let wrapper = format!(
            "cd {cwd} 2>>\"$0/err\" || {{ echo {EXIT_NO_CWD} >\"$0/exit\"; exit 1; }}; \
             {env}{cmd} <>\"$0/in\" >>\"$0/out.ndjson\" 2>>\"$0/err\" & \
             echo $! >\"$0/pid\"; wait $!; echo $? >\"$0/exit\"",
            cwd = quote_path(cwd),
        );
        let d = self.dir();
        let write_spec = spec_json
            .map(|j| format!("printf '%s\\n' {} >\"$d/spec.json\"\n", quote(j.trim())))
            .unwrap_or_default();
        // Waits for the PID, but gives up on a recorded exit (the wrapper
        // could not start the harness) or after `LAUNCH_WAIT_TICKS` ticks, so a
        // bad directory or a dead shell fails the launch rather than hanging it.
        format!(
            "set -e\n\
             {LOGIN_PATH}\
             d={d}\n\
             mkdir -p \"$d\"\n\
             [ -p \"$d/in\" ] || mkfifo \"$d/in\"\n\
             rm -f \"$d/pid\" \"$d/exit\"\n\
             : >>\"$d/out.ndjson\"\n\
             {write_spec}\
             nohup sh -c {wrapper} \"$d\" </dev/null >/dev/null 2>&1 &\n\
             i=0\n\
             while [ ! -s \"$d/pid\" ]; do\n\
             if [ -s \"$d/exit\" ]; then\n\
             echo \"harness did not start in {cwd_text}: $(cat \"$d/err\" 2>/dev/null | tail -n 1)\" >&2\n\
             exit {EXIT_NO_CWD}\n\
             fi\n\
             i=$((i + 1))\n\
             if [ \"$i\" -ge {LAUNCH_WAIT_TICKS} ]; then echo \"harness did not report a pid\" >&2; exit 1; fi\n\
             sleep 0.02\n\
             done\n\
             cat \"$d/pid\"\n",
            wrapper = quote(&wrapper),
            cwd_text = quote(cwd),
        )
    }

    /// Script that replaces `spec.json` (after the native id or title is known).
    pub fn write_spec_script(&self, spec_json: &str) -> String {
        format!(
            "d={}\n[ -d \"$d\" ] && printf '%s\\n' {} >\"$d/spec.json\"\n",
            self.dir(),
            quote(spec_json.trim())
        )
    }

    /// Script that streams the log from byte `offset` (0-based) while the
    /// harness runs.
    ///
    /// stdout carries log bytes only, so a client's offset is exactly the
    /// number of bytes it has received. stderr gets one `.` per second as a
    /// heartbeat; the write also notices a dead connection, so followers
    /// never outlive their client. Exits 0 once the harness has exited (the
    /// client then reads the rest with [`SessionDir::read_script`]), or
    /// [`EXIT_NOT_RUNNING`] if it never started.
    pub fn attach_script(&self, offset: u64) -> String {
        let d = self.dir();
        format!(
            "d={d}\n\
             pid=$(cat \"$d/pid\" 2>/dev/null) || exit {EXIT_NOT_RUNNING}\n\
             tail -c +{start} -f \"$d/out.ndjson\" &\n\
             t=$!\n\
             trap 'kill $t 2>/dev/null' EXIT\n\
             trap 'exit 1' HUP INT TERM\n\
             trap '' PIPE\n\
             while kill -0 \"$pid\" 2>/dev/null && kill -0 $t 2>/dev/null; do\n\
             printf . >&2 || exit 1\n\
             sleep 1\n\
             done\n\
             kill -0 \"$pid\" 2>/dev/null && exit 1\n\
             exit 0\n",
            start = offset + 1,
        )
    }

    /// Script that prints the log from byte `offset` to its current end.
    pub fn read_script(&self, offset: u64) -> String {
        format!("exec tail -c +{} {}/out.ndjson\n", offset + 1, self.dir())
    }

    /// Path of the session's log, as a shell expression.
    pub fn log_path(&self) -> String {
        format!("{}/out.ndjson", self.dir())
    }

    /// Path of the session's [`crate::watch`] events, as a shell expression.
    pub fn events_path(&self) -> String {
        format!("{}/events.ndjson", self.dir())
    }

    /// The directory, as a shell expression.
    pub(crate) fn dir_expr(&self) -> String {
        self.dir()
    }

    /// Path of the file the exit code is written to, as a shell expression.
    pub fn exit_path(&self) -> String {
        format!("{}/exit", self.dir())
    }

    /// Script whose stdin is forwarded to the harness as it arrives, for
    /// interactive use (`acc script input`) and clients that write whole
    /// lines. [`SessionDir::write_script`] also never forwards a partial
    /// write. Writers are serialized with the session lock; the FIFO is
    /// opened read-write, since a write-only open waits for a reader and
    /// input to a harness that already exited would hang. Exits
    /// [`EXIT_NOT_RUNNING`] when there is no session or it has exited.
    pub fn input_script(&self) -> String {
        let d = self.dir();
        format!(
            "d={d}\n\
             [ -p \"$d/in\" ] || {{ echo 'acc: no such session' >&2; exit {EXIT_NOT_RUNNING}; }}\n\
             [ -f \"$d/exit\" ] && {{ echo 'acc: the session has exited' >&2; exit {EXIT_NOT_RUNNING}; }}\n\
             {LOCK}\
             cat 1<>\"$d/in\"\n"
        )
    }

    /// Script that delivers exactly `len` bytes from stdin to the harness,
    /// at most once per `write_id`.
    ///
    /// The bytes are staged in a file and only forwarded once all of them
    /// arrived, so a connection that drops mid-upload never leaves half a
    /// JSON line in the harness's input ([`EXIT_INCOMPLETE`]). A retry with the
    /// same `write_id` after a lost acknowledgement is a no-op. Writers from
    /// several clients are serialized with an atomic `mkdir` lock, since FIFO
    /// writes are only atomic up to PIPE_BUF (512 bytes on macOS) and `flock`
    /// isn't on macOS by default. Exits [`EXIT_NOT_RUNNING`] if the harness is gone.
    ///
    /// `write_id` must match `[A-Za-z0-9_-]+`.
    pub fn write_script(&self, len: usize, write_id: &str) -> String {
        let d = self.dir();
        let id = quote(write_id);
        format!(
            "d={d}\n\
             t=\"$d/w.$$\"\n\
             trap 'rm -f \"$t\"' EXIT\n\
             cat >\"$t\"\n\
             [ \"$(wc -c <\"$t\" | tr -d ' ')\" = {len} ] || exit {EXIT_INCOMPLETE}\n\
             pid=$(cat \"$d/pid\" 2>/dev/null) && kill -0 \"$pid\" 2>/dev/null || exit {EXIT_NOT_RUNNING}\n\
             {LOCK}\
             trap 'rm -f \"$t\"; rm -rf \"$d/lock\"' EXIT\n\
             grep -qx {id} \"$d/sent\" 2>/dev/null && exit 0\n\
             cat \"$t\" 1<>\"$d/in\"\n\
             echo {id} >>\"$d/sent\"\n"
        )
    }

    /// Script printing `alive <pid> <size>`, `exited <code> <size>` or
    /// `missing`, where `size` is the log's length in bytes. Parse it with
    /// [`parse_status`].
    pub fn status_script(&self) -> String {
        let d = self.dir();
        format!(
            "d={d}\n\
             s=$(wc -c <\"$d/out.ndjson\" 2>/dev/null | tr -d ' ' || echo 0)\n\
             if [ -f \"$d/exit\" ]; then echo \"exited $(cat \"$d/exit\") $s\";\n\
             elif [ -f \"$d/pid\" ] && kill -0 \"$(cat \"$d/pid\")\" 2>/dev/null; then echo \"alive $(cat \"$d/pid\") $s\";\n\
             elif [ -f \"$d/pid\" ]; then echo \"exited - $s\";\n\
             else echo missing; fi\n"
        )
    }

    /// Script that asks the harness to exit (SIGTERM). Exits
    /// [`EXIT_NOT_RUNNING`] if it isn't running.
    pub fn stop_script(&self) -> String {
        let d = self.dir();
        format!(
            "d={d}\n\
             pid=$(cat \"$d/pid\" 2>/dev/null) && kill \"$pid\" 2>/dev/null || exit {EXIT_NOT_RUNNING}\n"
        )
    }

    /// Script that stops the harness: TERM, then KILL after `grace_ms`.
    pub fn kill_script(&self, grace_ms: u64) -> String {
        let d = self.dir();
        let tries = grace_ms.div_ceil(100).max(1);
        format!(
            "d={d}\n\
             p=$(cat \"$d/pid\" 2>/dev/null || true)\n\
             if [ -n \"$p\" ] && kill -TERM \"$p\" 2>/dev/null; then\n\
             i=0; while kill -0 \"$p\" 2>/dev/null && [ $i -lt {tries} ]; do sleep 0.1; i=$((i+1)); done\n\
             kill -KILL \"$p\" 2>/dev/null || true\n\
             fi\n"
        )
    }

    /// Script that deletes the session's directory, unless its harness is
    /// still running ([`EXIT_NOT_RUNNING`] is not an error here; exit 1 is).
    pub fn remove_script(&self) -> String {
        let d = self.dir();
        format!(
            "d={d}\n\
             pid=$(cat \"$d/pid\" 2>/dev/null) && kill -0 \"$pid\" 2>/dev/null && exit 1\n\
             rm -rf \"$d\"\n"
        )
    }
}

/// Takes the session's write lock, stealing it from a holder that died
/// (a client whose connection dropped while it was writing).
const LOCK: &str = r#"until mkdir "$d/lock" 2>/dev/null; do
h=$(cat "$d/lock/pid" 2>/dev/null) && [ -n "$h" ] && ! kill -0 "$h" 2>/dev/null && rm -rf "$d/lock"
sleep 0.05
done
echo $$ >"$d/lock/pid"
trap 'rm -rf "$d/lock"' EXIT
trap 'exit 1' HUP INT TERM
"#;

/// `PATH` from the user's login shell, for commands run over SSH exec
/// ([`LOGIN_PATH`]). Falls back to the current `PATH` when the login shell fails.
pub fn login_path_prelude() -> &'static str {
    LOGIN_PATH
}

/// Quotes a path, leaving a leading `~/` for the shell to expand.
pub(crate) fn quote_path(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => format!("\"$HOME\"/{}", quote(rest)),
        None if path == "~" => "\"$HOME\"".to_owned(),
        None => quote(path),
    }
}

/// A detached session's status from [`SessionDir::status_script`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Status {
    Alive {
        pid: u32,
        size: u64,
    },
    /// `code` is `None` when the process died without the wrapper recording it.
    Exited {
        code: Option<i32>,
        size: u64,
    },
    Missing,
}

impl Status {
    /// Bytes in the session's log.
    pub fn size(&self) -> u64 {
        match self {
            Status::Alive { size, .. } | Status::Exited { size, .. } => *size,
            Status::Missing => 0,
        }
    }
}

/// Parses the output of [`SessionDir::status_script`].
pub fn parse_status(out: &str) -> Option<Status> {
    let mut parts = out.split_whitespace();
    let num = |s: Option<&str>| s.and_then(|n| n.parse::<u64>().ok());
    match parts.next()? {
        "alive" => Some(Status::Alive {
            pid: parts.next()?.parse().ok()?,
            size: num(parts.next()).unwrap_or(0),
        }),
        "exited" => Some(Status::Exited {
            code: parts.next().and_then(|c| c.parse().ok()),
            size: num(parts.next()).unwrap_or(0),
        }),
        "missing" => Some(Status::Missing),
        _ => None,
    }
}

/// One session found by [`list_script`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Listing {
    pub id: String,
    pub status: Status,
    /// The log's last modification, in seconds since the Unix epoch.
    pub modified: Option<u64>,
    /// The `spec.json` line, when the session has one.
    pub spec_json: Option<String>,
}

/// Script listing every detached session on the host. Parse it with [`parse_list`].
pub fn list_script() -> String {
    format!(
        "for d in \"$HOME\"/{ROOT}/*/; do\n\
         [ -d \"$d\" ] || continue\n\
         d=${{d%/}}; id=${{d##*/}}\n\
         s=$(wc -c <\"$d/out.ndjson\" 2>/dev/null | tr -d ' ')\n\
         m=$(stat -c %Y \"$d/out.ndjson\" 2>/dev/null || stat -f %m \"$d/out.ndjson\" 2>/dev/null || echo -)\n\
         if [ -f \"$d/exit\" ]; then st=\"exited $(cat \"$d/exit\")\";\n\
         elif [ -f \"$d/pid\" ] && kill -0 \"$(cat \"$d/pid\")\" 2>/dev/null; then st=\"alive $(cat \"$d/pid\")\";\n\
         elif [ -f \"$d/pid\" ]; then st=\"exited -\";\n\
         else st=missing; fi\n\
         printf 'S\\t%s\\t%s %s\\t%s\\n' \"$id\" \"$st\" \"${{s:-0}}\" \"$m\"\n\
         [ -f \"$d/spec.json\" ] && printf 'J\\t%s\\n' \"$(head -n 1 \"$d/spec.json\")\"\n\
         done; true\n"
    )
}

/// Parses the output of [`list_script`].
pub fn parse_list(out: &str) -> Vec<Listing> {
    let mut list: Vec<Listing> = Vec::new();
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("S\t") {
            let mut f = rest.splitn(3, '\t');
            let (Some(id), Some(status), modified) = (f.next(), f.next(), f.next()) else {
                continue;
            };
            if SessionDir::new(id).is_none() {
                continue;
            }
            list.push(Listing {
                id: id.to_owned(),
                status: parse_status(status).unwrap_or(Status::Missing),
                modified: modified.and_then(|m| m.trim().parse().ok()),
                spec_json: None,
            });
        } else if let Some(json) = line.strip_prefix("J\t")
            && let Some(last) = list.last_mut()
        {
            last.spec_json = Some(json.to_owned());
        }
    }
    list
}

/// A file followed by [`fan_in_script`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FeedEntry {
    /// Tag for this file's lines, `[A-Za-z0-9_.:-]{1,128}`.
    pub key: String,
    /// The file, as a shell expression from [`SessionDir::log_path`] or
    /// [`SessionDir::exit_path`], or an absolute path (quoted here).
    pub path: FeedPath,
    /// Byte offset to resume from.
    pub offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum FeedPath {
    /// The log of a detached session.
    SessionLog(String),
    /// The exit-code file of a detached session.
    SessionExit(String),
    /// The events a [`crate::watch`] follower recorded for a detached session.
    SessionEvents(String),
    /// Any file, such as a harness transcript (`~/` is expanded).
    File(String),
}

fn valid_key(key: &str) -> bool {
    (1..=128).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))
}

/// One long-running script that follows many line-oriented files at once
/// and prints each line as `<key>\t<line>`. A phone uses one SSH channel
/// for every session on a host instead of one per session, which keeps it
/// under sshd's per-connection session limit.
///
/// Closing the channel signals the script, which takes every process it
/// started (its descendants, found with `ps`) down with it. Entries with
/// invalid keys or session ids are skipped.
pub fn fan_in_script(entries: &[FeedEntry]) -> String {
    let mut script = String::from(
        "k() { for c in $(ps -eo pid=,ppid= | awk -v p=\"$1\" '$2 == p { print $1 }'); do echo \"$c\"; k \"$c\"; done; }\n\
         trap 'trap - TERM INT HUP; kill $(k $$) 2>/dev/null' TERM INT HUP\n\
         f() { tail -c +\"$3\" -F \"$2\" 2>/dev/null | awk -v k=\"$1\" '{ print k \"\\t\" $0; fflush() }'; }\n",
    );
    for e in entries.iter().filter(|e| valid_key(&e.key)) {
        let path = match &e.path {
            FeedPath::SessionLog(id) => SessionDir::new(id).map(|d| d.log_path()),
            FeedPath::SessionExit(id) => SessionDir::new(id).map(|d| d.exit_path()),
            FeedPath::SessionEvents(id) => SessionDir::new(id).map(|d| d.events_path()),
            FeedPath::File(p) => Some(quote_path(p)),
        };
        if let Some(path) = path {
            script.push_str(&format!("f {} {path} {} &\n", e.key, e.offset + 1));
        }
    }
    script.push_str("wait\n");
    script
}

/// Splits [`fan_in_script`] output back into per-file lines and tracks
/// each file's offset, so a restarted script resumes without loss.
#[derive(Debug, Default)]
pub struct FanInParser {
    buf: Vec<u8>,
    offsets: std::collections::HashMap<String, u64>,
}

/// One line of one followed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FanInLine {
    pub key: String,
    /// The line's bytes, newline included, exactly as in the file.
    pub bytes: Vec<u8>,
    /// The file offset just past this line.
    pub offset: u64,
}

impl FanInParser {
    /// Starts tracking from the entries' offsets.
    pub fn new(entries: &[FeedEntry]) -> Self {
        Self {
            buf: Vec::new(),
            offsets: entries.iter().map(|e| (e.key.clone(), e.offset)).collect(),
        }
    }

    pub fn offset(&self, key: &str) -> Option<u64> {
        self.offsets.get(key).copied()
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<FanInLine> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut start = 0;
        while let Some(pos) = self.buf[start..].iter().position(|&b| b == b'\n') {
            let line = &self.buf[start..start + pos];
            start += pos + 1;
            let Some(tab) = line.iter().position(|&b| b == b'\t') else {
                continue;
            };
            let Ok(key) = std::str::from_utf8(&line[..tab]) else {
                continue;
            };
            let mut body = line[tab + 1..].to_vec();
            body.push(b'\n');
            let offset = self.offsets.entry(key.to_owned()).or_insert(0);
            *offset += body.len() as u64;
            out.push(FanInLine {
                key: key.to_owned(),
                bytes: body,
                offset: *offset,
            });
        }
        self.buf.drain(..start);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Harness;

    #[test]
    fn rejects_unsafe_ids() {
        assert!(SessionDir::new("../etc").is_none());
        assert!(SessionDir::new("").is_none());
        assert!(SessionDir::new("a b").is_none());
        assert!(SessionDir::new("s_01-abc").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn input_to_an_exited_session_does_not_hang() {
        let home = std::env::temp_dir().join(format!("acc-input-{}", std::process::id()));
        let d = home.join(ROOT).join("s1");
        std::fs::create_dir_all(&d).unwrap();
        let run = |script: String| {
            std::process::Command::new("timeout")
                .args(["5", "sh", "-c", &script])
                .env("HOME", &home)
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap()
                .status
                .code()
        };
        let dir = SessionDir::new("s1").unwrap();
        assert_eq!(run(format!("mkfifo {}", d.join("in").display())), Some(0));
        // No reader and no exit file yet: still returns (read-write open).
        assert_eq!(run(dir.input_script()), Some(0));
        std::fs::write(d.join("exit"), "1\n").unwrap();
        assert_eq!(run(dir.input_script()), Some(EXIT_NOT_RUNNING as i32));
        assert!(!d.join("lock").exists(), "the lock is released");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn launch_script_quotes_cwd_and_opens_fifo_read_write() {
        let dir = SessionDir::new("s1").expect("valid id");
        let script = dir.launch_script(&Harness::Pi.launch(None), "/tmp/my project");
        assert!(script.contains("mkfifo"));
        assert!(script.contains("<>"));
        assert!(script.contains("PI_PROGRAM_STATUS=1"));
        assert!(script.contains("my project"));
    }

    #[test]
    fn status_and_list_parse() {
        assert_eq!(
            parse_status("alive 42 1000\n"),
            Some(Status::Alive { pid: 42, size: 1000 })
        );
        assert_eq!(
            parse_status("exited 0 7"),
            Some(Status::Exited {
                code: Some(0),
                size: 7
            })
        );
        assert_eq!(
            parse_status("exited - 7"),
            Some(Status::Exited { code: None, size: 7 })
        );
        assert_eq!(parse_status("missing"), Some(Status::Missing));
        let list = parse_list(
            "S\ta1\talive 9 12\t1700000000\nJ\t{\"id\":\"a1\"}\nS\t../x\tmissing 0\t-\nS\tb2\texited 1 3\t-\n",
        );
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].spec_json.as_deref(), Some("{\"id\":\"a1\"}"));
        assert_eq!(list[0].modified, Some(1_700_000_000));
        assert_eq!(
            list[1].status,
            Status::Exited {
                code: Some(1),
                size: 3
            }
        );
    }

    #[test]
    fn fan_in_parser_tracks_offsets_across_chunks() {
        let mut p = FanInParser::new(&[FeedEntry {
            key: "a".into(),
            path: FeedPath::SessionLog("a".into()),
            offset: 10,
        }]);
        assert!(p.feed(b"a\t{\"x\"").is_empty());
        let lines = p.feed(b":1}\nb\thi\n");
        assert_eq!(lines[0].bytes, b"{\"x\":1}\n");
        assert_eq!(lines[0].offset, 18);
        assert_eq!((lines[1].key.as_str(), lines[1].offset), ("b", 3));
    }

    #[test]
    fn attach_offset_is_one_based_for_tail() {
        let dir = SessionDir::new("s1").expect("valid id");
        assert!(dir.attach_script(0).contains("tail -c +1 -f"));
        assert!(dir.attach_script(100).contains("tail -c +101 -f"));
    }
}

/// End-to-end check of the scripts with a real `sh`. Unix only.
#[cfg(all(test, unix))]
#[allow(clippy::expect_used)]
mod sh_tests {
    use super::*;
    use crate::harness::{LaunchSpec, Protocol};
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn sh(home: &std::path::Path, script: &str, stdin: &[u8]) -> String {
        sh_code(home, script, stdin).0
    }

    fn sh_code(home: &std::path::Path, script: &str, stdin: &[u8]) -> (String, Option<i32>) {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(script)
            .env("HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn sh");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(stdin)
            .expect("write");
        let out = child.wait_with_output().expect("wait");
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            out.status.code(),
        )
    }

    fn wait_for(path: &std::path::Path, lines: usize) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut content = String::new();
        while Instant::now() < deadline {
            content = std::fs::read_to_string(path).unwrap_or_default();
            if content.lines().count() >= lines {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        content
    }

    #[test]
    fn launch_fails_fast_when_the_directory_is_missing() {
        let home = std::env::temp_dir().join(format!("acc-test-nocwd-{}", std::process::id()));
        std::fs::create_dir_all(&home).expect("mkdir");
        let dir = SessionDir::new("nocwd").expect("valid id");
        let spec = LaunchSpec {
            program: "cat".into(),
            args: vec![],
            env: vec![],
            protocol: Protocol::Acp,
        };
        let missing = home.join("no-such-directory");
        let started = Instant::now();
        let (out, code) = sh_code(&home, &dir.launch_script(&spec, &missing.to_string_lossy()), b"");
        assert_eq!(code, Some(EXIT_NO_CWD as i32), "stdout: {out}");
        assert!(started.elapsed() < Duration::from_secs(5), "launch hung");
        assert!(out.trim().is_empty(), "no pid may be printed: {out}");
        let listed = parse_list(&sh(&home, &list_script(), b""));
        assert_eq!(listed.len(), 1);
        assert!(
            matches!(listed[0].status, Status::Exited { .. }),
            "{:?}",
            listed[0].status
        );
    }

    #[test]
    fn writes_are_whole_and_at_most_once() {
        let home = std::env::temp_dir().join(format!("acc-test-w-{}", std::process::id()));
        std::fs::create_dir_all(&home).expect("mkdir");
        let dir = SessionDir::new("w").expect("valid id");
        let spec = LaunchSpec {
            program: "cat".into(),
            args: vec![],
            env: vec![],
            protocol: Protocol::Acp,
        };
        sh(
            &home,
            &dir.launch_script_with_spec(&spec, "/", Some("{\"cwd\":\"/\"}")),
            b"",
        );
        let log = home.join(ROOT).join("w/out.ndjson");

        // A truncated upload is refused and nothing reaches the harness.
        let (_, code) = sh_code(&home, &dir.write_script(10, "a"), b"{\"n\":");
        assert_eq!(code, Some(EXIT_INCOMPLETE as i32));
        // The same write id twice is delivered once.
        for _ in 0..2 {
            let (_, code) = sh_code(&home, &dir.write_script(8, "b"), b"{\"n\":1}\n");
            assert_eq!(code, Some(0));
        }
        sh(&home, &dir.write_script(8, "c"), b"{\"n\":2}\n");
        assert_eq!(wait_for(&log, 2), "{\"n\":1}\n{\"n\":2}\n");

        // The follower replays from an offset and ends when the harness exits.
        let listed = parse_list(&sh(&home, &list_script(), b""));
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status.size(), 16);
        assert_eq!(listed[0].spec_json.as_deref(), Some("{\"cwd\":\"/\"}"));
        assert_eq!(sh_code(&home, &dir.stop_script(), b"").1, Some(0));
        let (out, code) = sh_code(&home, &dir.attach_script(8), b"");
        assert_eq!(code, Some(0));
        assert!("{\"n\":2}\n".starts_with(&out), "{out:?}");
        assert_eq!(sh(&home, &dir.read_script(8), b""), "{\"n\":2}\n");
        assert!(matches!(
            parse_status(&sh(&home, &dir.status_script(), b"")),
            Some(Status::Exited { size: 16, .. })
        ));
        let (_, code) = sh_code(&home, &dir.write_script(1, "d"), b"x");
        assert_eq!(code, Some(EXIT_NOT_RUNNING as i32));
        assert_eq!(sh_code(&home, &dir.remove_script(), b"").1, Some(0));
        assert!(parse_list(&sh(&home, &list_script(), b"")).is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn echo_harness_survives_writer_disconnects() {
        let home = std::env::temp_dir().join(format!("acc-test-{}", std::process::id()));
        std::fs::create_dir_all(&home).expect("mkdir");
        let dir = SessionDir::new("echo").expect("valid id");
        // `cat` stands in for a harness: it echoes each input line to the log.
        let spec = LaunchSpec {
            program: "cat".into(),
            args: vec![],
            env: vec![],
            protocol: Protocol::Acp,
        };
        let pid = sh(&home, &dir.launch_script(&spec, "/"), b"");
        assert!(!pid.trim().is_empty());

        // Two separate writers, as if a client disconnected and reconnected.
        sh(&home, &dir.input_script(), b"{\"n\":1}\n");
        sh(&home, &dir.input_script(), b"{\"n\":2}\n");

        let log = home.join(ROOT).join("echo/out.ndjson");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut content = String::new();
        while Instant::now() < deadline {
            content = std::fs::read_to_string(&log).unwrap_or_default();
            if content.lines().count() >= 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(content, "{\"n\":1}\n{\"n\":2}\n");
        assert!(sh(&home, &dir.status_script(), b"").starts_with("alive"));

        let _ = Command::new("kill").arg(pid.trim()).status();
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn spec_list_status_kill_and_remove() {
        let home = std::env::temp_dir().join(format!("acc-test-list-{}", std::process::id()));
        std::fs::create_dir_all(&home).expect("mkdir");
        let dir = SessionDir::new("lst").expect("valid id");
        let spec = LaunchSpec {
            program: "cat".into(),
            args: vec![],
            env: vec![],
            protocol: Protocol::Acp,
        };
        let pid = sh(
            &home,
            &dir.launch_script_with_spec(&spec, "~", Some("{\"id\":\"lst\",\"q\":\"it's\"}")),
            b"",
        );
        sh(&home, &dir.input_script(), b"0123456789\n");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline
            && !matches!(
                parse_status(&sh(&home, &dir.status_script(), b"")),
                Some(Status::Alive { size: 11, .. })
            )
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            parse_status(&sh(&home, &dir.status_script(), b"")),
            Some(Status::Alive {
                pid: pid.trim().parse().expect("pid"),
                size: 11
            })
        );
        let list = parse_list(&sh(&home, &list_script(), b""));
        assert_eq!(list.len(), 1);
        assert_eq!(
            list[0].spec_json.as_deref(),
            Some("{\"id\":\"lst\",\"q\":\"it's\"}")
        );

        sh(&home, &dir.kill_script(1_000), b"");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline
            && matches!(
                parse_status(&sh(&home, &dir.status_script(), b"")),
                Some(Status::Alive { .. })
            )
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(matches!(
            parse_status(&sh(&home, &dir.status_script(), b"")),
            Some(Status::Exited { size: 11, .. })
        ));
        sh(&home, &dir.remove_script(), b"");
        assert!(parse_list(&sh(&home, &list_script(), b"")).is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn fan_in_follows_files_from_offsets_and_dies_with_its_channel() {
        let home = std::env::temp_dir().join(format!("acc-test-fan-{}", std::process::id()));
        std::fs::create_dir_all(&home).expect("mkdir");
        let a = home.join("a.jsonl");
        let b = home.join("b b.jsonl");
        std::fs::write(&a, "{\"a\":0}\n{\"a\":1}\n").expect("write");
        std::fs::write(&b, "{\"b\":0}\n").expect("write");
        let entries = [
            FeedEntry {
                key: "ka".into(),
                path: FeedPath::File(a.display().to_string()),
                offset: 8,
            },
            FeedEntry {
                key: "kb".into(),
                path: FeedPath::File(b.display().to_string()),
                offset: 0,
            },
        ];
        let mut child = Command::new("sh");
        // Its own process group, as sshd gives every session.
        std::os::unix::process::CommandExt::process_group(&mut child, 0);
        let mut child = child
            .arg("-c")
            .arg(fan_in_script(&entries))
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn");
        let mut stdout = child.stdout.take().expect("stdout");
        let reader = std::thread::spawn(move || {
            let mut parser = FanInParser::new(&entries);
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            while got.len() < 3 {
                let n = std::io::Read::read(&mut stdout, &mut buf).expect("read");
                if n == 0 {
                    break;
                }
                got.extend(parser.feed(&buf[..n]));
            }
            (got, parser.offset("ka"))
        });
        std::thread::sleep(Duration::from_millis(300));
        let mut f = std::fs::OpenOptions::new().append(true).open(&b).expect("open");
        f.write_all(b"{\"b\":1}\n").expect("append");
        let (got, ka) = reader.join().expect("join");
        let mut lines: Vec<(String, String)> = got
            .iter()
            .map(|l| (l.key.clone(), String::from_utf8_lossy(&l.bytes).trim().to_owned()))
            .collect();
        lines.sort();
        assert_eq!(
            lines,
            [
                ("ka".to_owned(), "{\"a\":1}".to_owned()),
                ("kb".to_owned(), "{\"b\":0}".to_owned()),
                ("kb".to_owned(), "{\"b\":1}".to_owned()),
            ]
        );
        assert_eq!(ka, Some(16));
        // Closing the channel: TERM the script; its tails must go too.
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(child.id().to_string())
            .status();
        let _ = child.wait();
        std::thread::sleep(Duration::from_millis(200));
        let ps = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "ps -eo args | grep -F {} | grep -v grep || true",
                quote(&b.display().to_string())
            ))
            .output()
            .expect("ps");
        assert!(
            String::from_utf8_lossy(&ps.stdout).trim().is_empty(),
            "tail left running"
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}
