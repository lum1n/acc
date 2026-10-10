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
use acc_core::command::{Command as AccCommand, CommandEntry, OptionDef};
use acc_core::driver::{Driver, DriverError};
use acc_core::harness::{Harness, LaunchSpec, Protocol};
use acc_core::ndjson::{Frame, LineFramer};
use acc_ssh::{Auth, Detached, Error, Host, HostKeyCheck, Read, Retry, Session, SshConfig, Status};
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::time::timeout;

struct Sshd {
    dir: PathBuf,
    port: u16,
    child: Child,
    key: String,
    fingerprint: String,
}

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn find(name: &str) -> Option<PathBuf> {
    [
        "/usr/sbin",
        "/usr/bin",
        "/usr/local/sbin",
        "/usr/local/bin",
        "/opt/homebrew/sbin",
        "/sbin",
    ]
    .iter()
    .map(|d| Path::new(d).join(name))
    .find(|p| p.exists())
}

fn keygen(path: &Path) {
    let ok = Command::new(find("ssh-keygen").expect("ssh-keygen"))
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(path)
        .status()
        .expect("ssh-keygen")
        .success();
    assert!(ok);
}

fn start_sshd(name: &str) -> Option<Sshd> {
    let (Some(sshd), Some(_)) = (find("sshd"), find("ssh-keygen")) else {
        assert!(
            std::env::var_os("ACC_REQUIRE_SSHD").is_none(),
            "sshd and ssh-keygen are required"
        );
        eprintln!("skipping: sshd not installed");
        return None;
    };
    let dir = std::env::temp_dir().join(format!("acc-sshd-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    keygen(&dir.join("host"));
    keygen(&dir.join("user"));
    std::fs::copy(dir.join("user.pub"), dir.join("authorized_keys")).expect("authorized_keys");
    let fp = Command::new(find("ssh-keygen").expect("ssh-keygen"))
        .arg("-lf")
        .arg(dir.join("host.pub"))
        .output()
        .expect("fingerprint");
    let fingerprint = String::from_utf8_lossy(&fp.stdout)
        .split_whitespace()
        .nth(1)
        .expect("fingerprint")
        .to_owned();
    // Pick a free port.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let config = format!(
        "Port {port}\nListenAddress 127.0.0.1\nHostKey {d}/host\nAuthorizedKeysFile {d}/authorized_keys\n\
         PasswordAuthentication no\nKbdInteractiveAuthentication no\nStrictModes no\nPidFile {d}/pid\n\
         PermitRootLogin prohibit-password\nLogLevel ERROR\n",
        d = dir.display()
    );
    std::fs::write(dir.join("sshd_config"), config).expect("config");
    let child = Command::new(sshd)
        .args(["-D", "-e", "-f"])
        .arg(dir.join("sshd_config"))
        .stdin(Stdio::null())
        .spawn()
        .expect("spawn sshd");
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let key = std::fs::read_to_string(dir.join("user")).expect("key");
    Some(Sshd {
        dir,
        port,
        child,
        key,
        fingerprint,
    })
}

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
    let user = std::env::var("USER")
        .ok()
        .or_else(|| {
            let out = Command::new("id").arg("-un").output().ok()?;
            Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
        })
        .expect("user");
    let mut config = SshConfig::new("127.0.0.1", user, Auth::key(&sshd.key, None).expect("key"), check);
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

/// Answers each `{"prompt": ...}` line with an `{"echo": ...}` line, so a
/// shell `sed` can stand in for a harness.
struct EchoDriver {
    lines: LineFramer,
}

impl EchoDriver {
    fn boxed() -> Box<dyn Driver> {
        Box::new(Self {
            lines: LineFramer::new(1 << 20),
        })
    }
}

impl Driver for EchoDriver {
    fn harness(&self) -> Harness {
        Harness::Pi
    }

    fn launch(&self, _resume: Option<&str>) -> LaunchSpec {
        LaunchSpec {
            program: "sed".into(),
            args: vec!["-u".into(), "s/prompt/echo/".into()],
            env: vec![],
            protocol: Protocol::PiRpc,
        }
    }

    fn handshake(&mut self) -> Vec<u8> {
        b"{\"prompt\":\"hello\"}\n".to_vec()
    }

    fn on_output(&mut self, bytes: &[u8]) -> Vec<Event> {
        self.lines
            .feed(bytes)
            .into_iter()
            .filter_map(|f| match f {
                Frame::Line(message) => Some(Event::Notice { message }),
                _ => None,
            })
            .collect()
    }

    fn command(&mut self, cmd: AccCommand) -> Result<Vec<u8>, DriverError> {
        match cmd {
            AccCommand::Prompt { text } => {
                Ok(format!("{{\"prompt\":{}}}\n", serde_json::json!(text)).into_bytes())
            }
            _ => Err(DriverError::Unsupported {
                harness: Harness::Pi,
                what: "anything but prompts",
            }),
        }
    }

    fn commands(&self) -> &[CommandEntry] {
        &[]
    }

    fn options(&self) -> &[OptionDef] {
        &[]
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_drives_a_driver_across_reconnects() {
    let Some(sshd) = start_sshd("session") else { return };
    let proxy = Proxy::start(sshd.port).await;
    let h = host(
        &sshd,
        proxy.port,
        HostKeyCheck::Fingerprints(vec![sshd.fingerprint.clone()]),
    );
    let id = unique("session");
    let s = Session::launch(h.clone(), &id, EchoDriver::boxed(), "/tmp", None)
        .await
        .expect("launch");
    let next = || async {
        timeout(Duration::from_secs(20), s.next_events())
            .await
            .expect("timed out")
            .expect("events")
    };
    let notice = |text: &str| Event::Notice { message: text.into() };
    assert_eq!(next().await, vec![notice("{\"echo\":\"hello\"}")]);

    s.send(AccCommand::Prompt { text: "a b".into() })
        .await
        .expect("send");
    assert_eq!(next().await, vec![notice("{\"echo\":\"a b\"}")]);

    proxy.cut();
    s.send(AccCommand::Prompt { text: "after".into() })
        .await
        .expect("send after cut");
    assert_eq!(next().await, vec![notice("{\"echo\":\"after\"}")]);

    // A second client replays the whole session into a fresh driver.
    let other = Session::attach(h.clone(), &id, EchoDriver::boxed(), 0).expect("attach");
    let mut seen = Vec::new();
    while seen.len() < 3 {
        seen.extend(
            timeout(Duration::from_secs(20), other.next_events())
                .await
                .expect("timed out")
                .expect("events"),
        );
    }
    assert_eq!(seen[2], notice("{\"echo\":\"after\"}"));

    s.pipe().stop().await.expect("stop");
    assert!(matches!(next().await.as_slice(), [Event::Exited { .. }]));
    s.pipe().remove().await.expect("remove");
}
