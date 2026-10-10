//! Drives a real OpenSSH server on localhost, through a TCP proxy that can
//! cut every connection to simulate a phone losing its network.
//!
//! Skips when `sshd` or `ssh-keygen` is missing, unless `ACC_REQUIRE_SSHD=1`
//! (set in CI). Sessions live under the current user's `~/.acc/s` with
//! unique ids and are removed afterwards.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use acc_core::Event;
use acc_core::command::Command as AccCommand;
use acc_core::harness::{Harness, LaunchSpec, Protocol};
use acc_core::multiplexer::{Multiplexer, Target};
use acc_ssh::{
    AgentSocket, Auth, Detached, Error, Host, HostKeyCheck, Pane, Read, Retry, Session, SessionConfig,
    SshConfig, Status,
};
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::time::timeout;

#[path = "support/sshd.rs"]
mod support;

use support::{Sshd, find, keygen, start_sshd};

/// A TCP proxy to sshd. New connections are always accepted; existing ones
/// can be cut (like a phone switching networks) or frozen (like iOS
/// suspending the app: the socket stays open but nothing moves).
struct Proxy {
    port: u16,
    generation: watch::Sender<Fault>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    Cut(u32),
    Freeze(u32),
}

impl Proxy {
    async fn start(target: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let (generation, rx) = watch::channel(Fault::None);
        tokio::spawn(async move {
            while let Ok((mut inbound, _)) = listener.accept().await {
                let mut rx = rx.clone();
                rx.mark_unchanged();
                tokio::spawn(async move {
                    let Ok(mut outbound) = TcpStream::connect(("127.0.0.1", target)).await else {
                        return;
                    };
                    tokio::select! {
                        _ = copy_bidirectional(&mut inbound, &mut outbound) => return,
                        _ = rx.changed() => {}
                    }
                    if matches!(*rx.borrow(), Fault::Freeze(_)) {
                        // Hold both sockets open without forwarding anything.
                        let _ = rx.changed().await;
                        std::future::pending::<()>().await;
                    }
                });
            }
        });
        Self { port, generation }
    }

    fn cut(&self) {
        self.generation.send_modify(|f| *f = Fault::Cut(next(*f)));
    }

    fn freeze(&self) {
        self.generation.send_modify(|f| *f = Fault::Freeze(next(*f)));
    }
}

fn next(f: Fault) -> u32 {
    match f {
        Fault::None => 1,
        Fault::Cut(n) | Fault::Freeze(n) => n + 1,
    }
}

fn host(sshd: &Sshd, port: u16, check: HostKeyCheck) -> Arc<Host> {
    host_with(port, check, Auth::key(&sshd.key, None).expect("key"))
}

fn host_with(port: u16, check: HostKeyCheck, auth: Auth) -> Arc<Host> {
    let mut config = SshConfig::new("127.0.0.1", support::user(), auth, check);
    config.port = port;
    config.connect_timeout = Duration::from_secs(5);
    Arc::new(Host::new(config))
}

fn cat_spec() -> LaunchSpec {
    LaunchSpec {
        program: "cat".into(),
        args: vec![],
        env: vec![],
        protocol: Protocol::Acp,
    }
}

fn unique(name: &str) -> String {
    format!("t-{name}-{}", std::process::id())
}

fn retry() -> Retry {
    Retry {
        attempts: 20,
        backoff: Duration::from_millis(50),
        max_backoff: Duration::from_millis(500),
        stall: Duration::from_secs(3),
    }
}

/// Reads until `want` bytes arrived in total.
async fn read_exactly(d: &Detached, want: usize) -> Vec<u8> {
    let mut got = Vec::new();
    while got.len() < want {
        match timeout(Duration::from_secs(20), d.read())
            .await
            .expect("read timed out")
            .expect("read")
        {
            Read::Data(b) => got.extend(b),
            Read::Exited(c) => panic!("exited early ({c:?}) after {got:?}"),
        }
    }
    got
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_unknown_host_keys() {
    let Some(sshd) = start_sshd("hostkey") else { return };
    let h = host(
        &sshd,
        sshd.port,
        HostKeyCheck::Fingerprints(vec!["SHA256:nope".into()]),
    );
    match h.connect().await {
        Err(Error::HostKey { fingerprint }) => assert_eq!(fingerprint, sshd.fingerprint),
        other => panic!("expected a host key error, got {other:?}"),
    }
    let h = host(
        &sshd,
        sshd.port,
        HostKeyCheck::Fingerprints(vec![sshd.fingerprint.clone()]),
    );
    let out = h
        .exec("echo \"$0\" ok", b"")
        .await
        .expect("exec")
        .ok()
        .expect("exit 0");
    assert_eq!(out.stdout_str(), "/bin/sh ok\n");
    assert_eq!(
        h.host_key_fingerprint().as_deref(),
        Some(sshd.fingerprint.as_str())
    );
}

/// An `ssh-agent` on its own socket, killed on drop.
struct Agent {
    child: Child,
    socket: PathBuf,
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Agent {
    fn start(dir: &Path) -> Self {
        let socket = dir.join("agent.sock");
        let child = Command::new(find("ssh-agent").expect("ssh-agent"))
            .arg("-D")
            .arg("-a")
            .arg(&socket)
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn ssh-agent");
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Self { child, socket }
    }

    fn add(&self, key: &Path) {
        let ok = Command::new(find("ssh-add").expect("ssh-add"))
            .arg(key)
            .env("SSH_AUTH_SOCK", &self.socket)
            .stderr(Stdio::null())
            .status()
            .expect("ssh-add")
            .success();
        assert!(ok, "ssh-add {}", key.display());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authenticates_with_ssh_agent() {
    let Some(sshd) = start_sshd("agent") else { return };
    let agent = Agent::start(&sshd.dir);
    let check = || HostKeyCheck::Fingerprints(vec![sshd.fingerprint.clone()]);
    let via_agent = |socket: &Path| {
        host_with(
            sshd.port,
            check(),
            Auth::Agent(AgentSocket::Path(socket.to_owned())),
        )
    };

    // An empty agent has nothing to offer.
    match via_agent(&agent.socket).connect().await {
        Err(Error::Auth { .. }) => {}
        other => panic!("expected an auth error, got {other:?}"),
    }
    // A missing agent is its own error, not a rejection by the host.
    match via_agent(&sshd.dir.join("no-agent.sock")).connect().await {
        Err(Error::Agent(_)) => {}
        other => panic!("expected an agent error, got {other:?}"),
    }

    // The host rejects the first key and accepts the second.
    keygen(&sshd.dir.join("other"));
    agent.add(&sshd.dir.join("other"));
    agent.add(&sshd.dir.join("user"));
    let h = via_agent(&agent.socket);
    let out = h.exec("echo ok", b"").await.expect("exec").ok().expect("exit 0");
    assert_eq!(out.stdout_str(), "ok\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detached_session_survives_dropped_connections() {
    let Some(sshd) = start_sshd("drop") else { return };
    let proxy = Proxy::start(sshd.port).await;
    let check = HostKeyCheck::Fingerprints(vec![sshd.fingerprint.clone()]);
    let phone = host(&sshd, proxy.port, check.clone());
    let id = unique("drop");

    let d = Detached::launch(phone.clone(), &id, &cat_spec(), "/")
        .await
        .expect("launch")
        .with_retry(retry());
    d.write(b"one\n").await.expect("write");
    assert_eq!(read_exactly(&d, 4).await, b"one\n");

    // The phone loses its network mid-session.
    proxy.cut();
    // Meanwhile the desktop, on its own connection, keeps the session going.
    let desktop = Detached::attach(host(&sshd, sshd.port, check), &id, 0)
        .expect("attach")
        .with_retry(retry());
    desktop.write(b"two\n").await.expect("desktop write");

    // The phone reads on from its offset: nothing lost, nothing repeated.
    d.write(b"three\n").await.expect("write after cut");
    assert_eq!(read_exactly(&d, 10).await, b"two\nthree\n");
    assert!(phone.connections() >= 2, "the phone redialed");
    assert_eq!(d.offset(), 14);
    assert_eq!(read_exactly(&desktop, 14).await, b"one\ntwo\nthree\n");

    // Cut again while a read is waiting.
    let reader = {
        let d = Arc::new(d);
        let r = d.clone();
        (tokio::spawn(async move { read_exactly(&r, 5).await }), d)
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    proxy.cut();
    reader.1.write(b"four\n").await.expect("write");
    assert_eq!(reader.0.await.expect("join"), b"four\n");
    let d = reader.1;

    // The connection freezes without closing: the follower's heartbeat stops,
    // so the reader redials instead of waiting for TCP to give up.
    let dials = phone.connections();
    proxy.freeze();
    d.write(b"five\n")
        .await
        .expect("write through a frozen connection");
    assert_eq!(read_exactly(&d, 5).await, b"five\n");
    assert!(phone.connections() > dials);

    // The listing shows it, with the spec a client needs to rebuild a driver.
    let listed = acc_ssh::list(&phone).await.expect("list");
    let me = listed.iter().find(|l| l.id == id).expect("listed");
    assert!(matches!(me.status, Status::Alive { .. }));
    assert_eq!(me.log_bytes, 24);
    assert_eq!(me.spec.as_ref().map(|s| s.spec.program.as_str()), Some("cat"));

    // Stopping drains the log, then reports the exit.
    d.stop().await.expect("stop");
    let last = timeout(Duration::from_secs(20), d.read())
        .await
        .expect("timed out")
        .expect("read");
    assert!(matches!(last, Read::Exited(_)), "unexpected {last:?}");
    assert!(matches!(d.write(b"x\n").await, Err(Error::NotRunning(_))));
    d.remove().await.expect("remove");
}

/// Stands in for Claude Code: replays a recorded session up to its
/// permission request, waits for the answer on stdin (logging every input
/// line to `inlog`), then replays the rest.
fn fake_claude(inlog: &Path) -> LaunchSpec {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../acc-core/tests/fixtures/claude/permission-deny.ndjson");
    let script = r#"head -n 20 "$1"
while IFS= read -r l; do printf '%s\n' "$l" >> "$2"; case $l in *ef3cff2f*) break;; esac; done
tail -n +21 "$1"
exec cat >> "$2""#;
    LaunchSpec {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            script.into(),
            "sh".into(),
            fixture.display().to_string(),
            inlog.display().to_string(),
        ],
        env: vec![],
        protocol: Protocol::ClaudeStreamJson,
    }
}

/// Reads events until one matches `stop`, returning all of them.
async fn events_until(s: &Session, stop: impl Fn(&Event) -> bool) -> Vec<Event> {
    let mut seen = Vec::new();
    loop {
        let got = timeout(Duration::from_secs(20), s.next_events())
            .await
            .expect("timed out")
            .expect("events");
        let done = got.iter().any(&stop);
        seen.extend(got);
        if done {
            return seen;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_drives_a_harness_across_reconnects() {
    let Some(sshd) = start_sshd("session") else { return };
    let proxy = Proxy::start(sshd.port).await;
    let h = host(
        &sshd,
        proxy.port,
        HostKeyCheck::Fingerprints(vec![sshd.fingerprint.clone()]),
    );
    let id = unique("session");
    let inlog = sshd.dir.join("claude-stdin");
    let config = SessionConfig::new(Harness::Claude, "/tmp");
    let mut core = acc_core::session::Session::new(config.clone());
    let pipe = Detached::launch(h.clone(), &id, &fake_claude(&inlog), "/tmp")
        .await
        .expect("launch")
        .with_retry(retry());
    pipe.write(&core.start()).await.expect("handshake");
    let s = Session::from_parts(pipe, core);

    let seen = events_until(&s, |e| matches!(e, Event::PermissionRequested { .. })).await;
    let Some(Event::PermissionRequested {
        request_id, options, ..
    }) = seen.last()
    else {
        panic!("no permission request in {seen:?}");
    };
    assert_eq!(
        s.native_id().as_deref(),
        Some("5f3c1a52-8a3e-4c1e-9d47-2b6f0e9a7c11")
    );
    assert!(!s.commands().is_empty(), "command palette from the handshake");
    let state = s.state().expect("state");
    assert!(state.observed_at_ms > 0, "state is stamped with the clock");

    // The phone loses its network just before the user answers.
    proxy.cut();
    let deny = options
        .iter()
        .find(|o| o.id.contains("deny") || o.id.contains("reject"))
        .expect("deny option");
    s.send(AccCommand::AnswerPermission {
        request_id: request_id.clone(),
        option_id: deny.id.clone(),
    })
    .await
    .expect("answer after cut");
    events_until(&s, |e| matches!(e, Event::TurnCompleted)).await;

    // A second client replays the whole session into a fresh core.
    let other = Session::attach(h.clone(), &id, config, 0).expect("attach");
    let replay = events_until(&other, |e| matches!(e, Event::TurnCompleted)).await;
    assert!(
        replay
            .iter()
            .any(|e| matches!(e, Event::PermissionRequested { .. }))
    );
    assert_eq!(other.native_id(), s.native_id());

    s.pipe().stop().await.expect("stop");
    let exit = events_until(&s, |e| matches!(e, Event::Exited { .. })).await;
    assert!(
        exit.iter()
            .any(|e| matches!(e, Event::StateChanged(st) if st.state == acc_core::RunState::Exited))
    );
    // Later reads only repeat the exit.
    let again = s.next_events().await.expect("events");
    assert!(matches!(again.as_slice(), [Event::Exited { .. }]), "{again:?}");

    // The answer reached the harness exactly once, after the handshake.
    let input = std::fs::read_to_string(&inlog).expect("stdin log");
    assert!(input.contains("\"initialize\""), "{input}");
    assert_eq!(input.matches(request_id.as_str()).count(), 1, "{input}");
    s.pipe().remove().await.expect("remove");
}

/// Stands in for Claude Code's TUI: once the pane is wide enough, shows a
/// recorded permission dialog and waits for one key, which it saves to
/// `answer`. A moment later it reports working over OSC 7501 and shows the
/// recorded working screen.
fn fake_claude_tui(answer: &Path) -> acc_core::multiplexer::Program {
    let screens = Path::new(env!("CARGO_MANIFEST_DIR")).join("../acc-core/tests/fixtures/screens");
    let script = r#"while [ "$(stty size | cut -d' ' -f2)" -lt 120 ]; do sleep 0.1; done
stty -icanon -echo min 1
cat "$1/claude-permission-bash.txt"
dd bs=1 count=1 of="$2" 2>/dev/null
sleep 1
printf '\033[2J\033[H\033]7501;state=working\033\\'
cat "$1/claude-working.txt"
exec cat >/dev/null"#;
    acc_core::multiplexer::Program {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            script.into(),
            "sh".into(),
            screens.display().to_string(),
            answer.display().to_string(),
        ],
        env: vec![],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn co_drives_a_tui_in_a_tmux_pane() {
    if !Path::new("/usr/bin/tmux").exists() && !Path::new("/opt/homebrew/bin/tmux").exists() {
        assert!(std::env::var_os("ACC_REQUIRE_MUX").is_none(), "tmux is required");
        eprintln!("skipping: tmux not installed");
        return;
    }
    let Some(sshd) = start_sshd("tmux") else { return };
    let proxy = Proxy::start(sshd.port).await;
    let h = host(
        &sshd,
        proxy.port,
        HostKeyCheck::Fingerprints(vec![sshd.fingerprint.clone()]),
    );
    let target = Target {
        multiplexer: Multiplexer::Tmux,
        server: sshd.dir.join("tmux.sock").display().to_string(),
        session: None,
    };
    let answer = sshd.dir.join("answer");
    let pane_ref = acc_ssh::pane::spawn(&h, &target, &fake_claude_tui(&answer), "/tmp", "acc-test")
        .await
        .expect("spawn");
    h.exec(
        &format!(
            "tmux -S {} resize-window -t {} -x 160 -y 50",
            target.server, pane_ref.pane
        ),
        b"",
    )
    .await
    .expect("resize")
    .ok()
    .expect("resize ok");
    let listed = acc_ssh::pane::list_panes(&h, &target).await.expect("list");
    assert!(listed.iter().any(|p| p.pane == pane_ref), "{listed:?}");

    let pane = Pane::new(h.clone(), pane_ref.clone(), Harness::Claude).with_pacing(acc_ssh::pane::Pacing {
        min_gap: Duration::from_millis(100),
        idle: Duration::from_millis(500),
    });
    let next = || async {
        timeout(Duration::from_secs(20), pane.next_events())
            .await
            .expect("timed out")
            .expect("events")
    };
    let mut seen = Vec::new();
    let request = loop {
        let got = next().await;
        if let Some(Event::PermissionRequested { request_id, .. }) = got
            .iter()
            .find(|e| matches!(e, Event::PermissionRequested { .. }))
        {
            break request_id.clone();
        }
        seen.extend(got);
    };
    assert!(matches!(
        pane.state().map(|s| s.state),
        Some(acc_core::RunState::Blocked { .. })
    ));

    // The phone drops off the network; the answer still lands.
    proxy.cut();
    pane.send(AccCommand::AnswerPermission {
        request_id: request.clone(),
        option_id: "1".into(),
    })
    .await
    .expect("answer");
    let mut osc = false;
    let mut resolved = false;
    while !(osc && resolved) {
        for e in next().await {
            match e {
                Event::StateChanged(s)
                    if s.source == acc_core::StateSource::Osc7501
                        && matches!(s.state, acc_core::RunState::Working { .. }) =>
                {
                    osc = true;
                }
                Event::PermissionResolved { request_id } if request_id == request => resolved = true,
                _ => {}
            }
        }
    }
    assert_eq!(std::fs::read_to_string(&answer).expect("answer"), "1");

    // A dialog that's gone can't be answered.
    let stale = pane
        .send(AccCommand::AnswerPermission {
            request_id: request,
            option_id: "1".into(),
        })
        .await;
    assert!(matches!(stale, Err(Error::Driver(_))), "{stale:?}");

    pane.kill().await.expect("kill");
    let exit = next().await;
    assert!(matches!(exit.as_slice(), [.., Event::Exited { .. }]), "{exit:?}");
    let _ = h
        .exec(&format!("tmux -S {} kill-server", target.server), b"")
        .await;
}
