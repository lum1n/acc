//! Daemonless, disconnect-proof sessions.
//!
//! The harness runs detached on the host with its stdin on a FIFO and its
//! stdout appended to a log. A client (desktop, or a phone over SSH) attaches
//! by tailing the log from a byte offset and writing to the FIFO. Only `sh`,
//! `mkdir`, `mkfifo`, `nohup`, `tail` and `cat` are used, so nothing is
//! installed on the host. See `research/daemonless-ssh-control.md` §2 and §4.
//!
//! Layout under `$HOME/.acc/s/<session>/`:
//! `in` (FIFO), `out.ndjson`, `err`, `pid`, `exit`, `spec.json`.

use crate::harness::LaunchSpec;
use crate::shell::{join, quote};

/// Root directory for detached sessions, relative to `$HOME`.
pub const ROOT: &str = ".acc/s";

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
    /// The FIFO is opened read-write (`<>`) so the harness never sees EOF
    /// when a client disconnects. `nohup` keeps it alive after SSH hangs up.
    /// Prints the harness PID on success.
    pub fn launch_script(&self, spec: &LaunchSpec, cwd: &str) -> String {
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
        // records the exit code. `$0` is the session directory.
        let wrapper = format!(
            "cd {cwd} || exit 1; {env}{cmd} <>\"$0/in\" >>\"$0/out.ndjson\" 2>>\"$0/err\" & \
             echo $! >\"$0/pid\"; wait $!; echo $? >\"$0/exit\"",
            cwd = quote(cwd),
        );
        let d = self.dir();
        format!(
            "set -e\n\
             d={d}\n\
             mkdir -p \"$d\"\n\
             [ -p \"$d/in\" ] || mkfifo \"$d/in\"\n\
             rm -f \"$d/pid\" \"$d/exit\"\n\
             : >>\"$d/out.ndjson\"\n\
             nohup sh -c {wrapper} \"$d\" </dev/null >/dev/null 2>&1 &\n\
             while [ ! -s \"$d/pid\" ]; do sleep 0.02; done\n\
             cat \"$d/pid\"\n",
            wrapper = quote(&wrapper),
        )
    }

    /// Script that streams the log from byte `offset` (0-based) and keeps following.
    pub fn attach_script(&self, offset: u64) -> String {
        format!("exec tail -c +{} -f {}/out.ndjson\n", offset + 1, self.dir())
    }

    /// Script whose stdin is forwarded to the harness. Writers are serialized
    /// with an atomic `mkdir` lock, since FIFO writes are only atomic up to
    /// PIPE_BUF (512 bytes on macOS) and `flock` isn't on macOS by default.
    pub fn input_script(&self) -> String {
        let d = self.dir();
        format!(
            "d={d}\n\
             until mkdir \"$d/lock\" 2>/dev/null; do sleep 0.05; done\n\
             trap 'rmdir \"$d/lock\"' EXIT\n\
             cat >\"$d/in\"\n"
        )
    }

    /// Script printing `alive <pid>`, `exited <code>` or `missing`.
    pub fn status_script(&self) -> String {
        let d = self.dir();
        format!(
            "d={d}\n\
             if [ -f \"$d/exit\" ]; then echo \"exited $(cat \"$d/exit\")\";\n\
             elif [ -f \"$d/pid\" ] && kill -0 \"$(cat \"$d/pid\")\" 2>/dev/null; then echo \"alive $(cat \"$d/pid\")\";\n\
             else echo missing; fi\n"
        )
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
        String::from_utf8_lossy(&out.stdout).into_owned()
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
}
