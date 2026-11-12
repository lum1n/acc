//! `Host::local`: the same detached sessions on this machine, with `sh` instead of SSH. Sessions
//! live under the current user's `~/.acc/s` with unique ids and are removed afterwards.

#![cfg(all(unix, feature = "local"))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use acc_core::harness::{LaunchSpec, Protocol};
use acc_ssh::{Detached, Error, Host, Read, Status, StreamMsg, list};
use tokio::time::timeout;

fn unique(name: &str) -> String {
    format!("t-local-{name}-{}", std::process::id())
}

#[tokio::test]
async fn runs_scripts_with_a_login_environment_only() {
    // Cargo sets this for the test process; a script on the host must not see it.
    assert!(std::env::var("CARGO_PKG_NAME").is_ok());
    let host = Host::local();
    assert!(host.is_local());
    host.connect().await.expect("nothing to dial");
    let out = host
        .exec(
            "read x; echo \"got $x\"; echo oops >&2; echo \"${CARGO_PKG_NAME:-none}\"; exit 3",
            b"hi\n",
        )
        .await
        .expect("exec");
    assert_eq!(out.stdout_str(), "got hi\nnone\n");
    assert_eq!(out.stderr, b"oops\n");
    assert_eq!(out.code, Some(3));
    let home = host.exec("echo \"$HOME\"", b"").await.expect("exec").stdout_str();
    assert_eq!(home.trim(), std::env::var("HOME").unwrap_or_default());
}

#[tokio::test]
async fn describes_itself_as_localhost() {
    let host = Host::local();
    assert_eq!(host.config().host, "localhost");
    assert!(!Host::new(host.config().clone()).is_local());
}

#[tokio::test]
async fn reports_a_signal_as_no_exit_code() {
    let out = Host::local().exec("kill -9 $$", b"").await.expect("exec");
    assert_eq!(out.code, None);
}

#[tokio::test]
async fn exec_ignores_input_the_script_never_reads() {
    // More than a pipe buffer, to a script that exits without reading it.
    let input = vec![b'x'; 1 << 20];
    let out = Host::local().exec("echo done", &input).await.expect("exec");
    assert_eq!(out.stdout_str(), "done\n");
    assert_eq!(out.code, Some(0));
}

/// Everything a stream sends until it ends: stdout, stderr and how it ended.
async fn drain(stream: &mut acc_ssh::Stream) -> (Vec<u8>, Vec<u8>, StreamMsg) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    loop {
        match timeout(Duration::from_secs(10), stream.recv())
            .await
            .expect("recv in time")
        {
            StreamMsg::Stdout(bytes) => out.extend(bytes),
            StreamMsg::Stderr(bytes) => err.extend(bytes),
            end => return (out, err, end),
        }
    }
}

#[tokio::test]
async fn streams_a_script_fed_through_its_stdin() {
    let host = Host::local();
    let mut stream = host
        .open("while read line; do echo \"out $line\"; echo \"err $line\" >&2; done; exit 7")
        .await
        .expect("open");
    stream.write(b"a\n").await.expect("write");
    stream.write(b"b\n").await.expect("write");
    stream.finish().await.expect("finish");
    let (out, err, end) = drain(&mut stream).await;
    assert_eq!(out, b"out a\nout b\n");
    assert_eq!(err, b"err a\nerr b\n");
    assert!(matches!(end, StreamMsg::Exited(Some(7))), "{end:?}");
}

#[tokio::test]
async fn refuses_writes_after_finish() {
    let host = Host::local();
    let mut stream = host.open("cat").await.expect("open");
    stream.finish().await.expect("finish");
    // Finishing twice is harmless.
    stream.finish().await.expect("finish again");
    assert!(matches!(stream.write(b"late").await, Err(Error::Disconnected)));
    let (out, _, end) = drain(&mut stream).await;
    assert!(out.is_empty());
    assert!(matches!(end, StreamMsg::Exited(Some(0))), "{end:?}");
}

#[tokio::test]
async fn streams_a_killed_script_as_exited_without_a_code() {
    let host = Host::local();
    let mut stream = host.open("echo up; kill -9 $$").await.expect("open");
    let (out, _, end) = drain(&mut stream).await;
    assert_eq!(out, b"up\n");
    assert!(matches!(end, StreamMsg::Exited(None)), "{end:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drives_a_detached_session_on_this_machine() {
    let host = Arc::new(Host::local());
    let id = unique("cat");
    let spec = LaunchSpec {
        program: "cat".into(),
        args: vec![],
        env: vec![],
        protocol: Protocol::Acp,
        sandbox: None,
    };
    let pipe = Detached::launch(host.clone(), &id, &spec, "/tmp", None)
        .await
        .expect("launch");
    pipe.write(b"hello\n").await.expect("write");
    let mut got = Vec::new();
    while !got.ends_with(b"hello\n") {
        match timeout(Duration::from_secs(10), pipe.read())
            .await
            .expect("read in time")
            .expect("read")
        {
            Read::Data(bytes) => got.extend(bytes),
            Read::Exited(code) => panic!("exited early with {code:?}"),
        }
    }
    assert!(list(&host).await.expect("list").iter().any(|l| l.id == id));
    assert!(matches!(
        pipe.status().await.expect("status"),
        Status::Alive { .. }
    ));

    // Another client attaches from the start and sees the same bytes.
    let other = Detached::attach(host.clone(), &id, 0).expect("attach");
    match timeout(Duration::from_secs(10), other.read())
        .await
        .expect("read in time")
        .expect("read")
    {
        Read::Data(bytes) => assert!(b"hello\n".starts_with(&bytes) || bytes.starts_with(b"hello")),
        Read::Exited(code) => panic!("exited early with {code:?}"),
    }

    pipe.stop().await.expect("stop");
    loop {
        match timeout(Duration::from_secs(10), pipe.read())
            .await
            .expect("read in time")
            .expect("read")
        {
            Read::Exited(_) => break,
            Read::Data(_) => {}
        }
    }
    pipe.remove().await.expect("remove");
}

#[tokio::test]
async fn exec_feeds_large_input_to_a_script_that_writes_as_it_reads() {
    // More than a pipe holds, both ways: input must be fed while output is drained.
    let input = vec![b'x'; 1 << 20];
    let out = timeout(Duration::from_secs(20), Host::local().exec("cat", &input))
        .await
        .expect("exec in time")
        .expect("exec");
    assert_eq!(out.stdout.len(), input.len());
    assert_eq!(out.code, Some(0));
}

/// Kills a detached session's harness and its wrapper without either
/// recording an exit, as a reboot does.
async fn kill_like_a_reboot(host: &Host, id: &str) {
    let d = format!("\"$HOME\"/.acc/s/{id}");
    host.exec(
        &format!(
            "p=$(cat {d}/pid); w=$(ps -o ppid= -p \"$p\" | tr -d ' '); kill -KILL \"$w\" \"$p\"; sleep 0.2"
        ),
        b"",
    )
    .await
    .expect("kill");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revives_an_interrupted_session_under_the_same_id() {
    use acc_core::harness::{Harness, Launcher};
    use acc_ssh::{Session, SessionSpec};

    let host = Arc::new(Host::local());
    let id = unique("revive");
    // `cat` stands in for Claude: it echoes what the session writes.
    let launcher = Launcher {
        prefix: vec!["sh".into(), "-c".into(), "exec cat".into(), "x".into()],
        program: None,
        args: vec![],
    };
    let launch = launcher.wrap(Harness::Claude.launch(None));
    let mut spec = SessionSpec::new(&id, Harness::Claude, "/tmp", launch.clone(), 7);
    spec.launcher = Some(launcher);
    spec.title = Some("Before the reboot".into());
    let pipe = Detached::launch(host.clone(), &id, &launch, "/tmp", Some(&spec.to_json()))
        .await
        .expect("launch");
    pipe.write(b"{\"type\":\"before\"}\n").await.expect("write");
    while pipe.status().await.expect("status").size() < 18 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    kill_like_a_reboot(&host, &id).await;
    let listed = list(&host).await.expect("list");
    let me = listed.iter().find(|l| l.id == id).expect("listed");
    assert_eq!(me.status, Status::Interrupted { size: 18 });

    // The harness never reported its session id: nothing to resume.
    assert!(matches!(
        Session::revive(host.clone(), &id, "t").await,
        Err(Error::NotResumable(_))
    ));

    spec.native_id = Some("abc".into());
    host.exec(
        &acc_core::detached::SessionDir::new(&id)
            .expect("id")
            .write_spec_script(&spec.to_json()),
        b"",
    )
    .await
    .expect("spec");
    let revived = Session::revive(host.clone(), &id, "t").await.expect("revive");
    assert_eq!(revived.pipe().offset(), 18, "reads only the new run");
    assert!(matches!(
        revived.pipe().status().await.expect("status"),
        Status::Alive { .. }
    ));
    let listed = list(&host).await.expect("list");
    let now = listed
        .iter()
        .find(|l| l.id == id)
        .and_then(|l| l.spec_json.as_deref())
        .and_then(SessionSpec::from_json)
        .expect("spec");
    assert_eq!(
        (now.created_ms, now.title.as_deref()),
        (7, Some("Before the reboot"))
    );
    assert!(
        now.launch.args.windows(2).any(|w| w == ["--resume", "abc"]),
        "{:?}",
        now.launch.args
    );
    assert!(
        now.launch.args.starts_with(&["-c".into(), "exec cat".into()]),
        "the launcher is kept"
    );

    // Running again: not revived twice.
    assert!(matches!(
        Session::revive(host.clone(), &id, "t").await,
        Err(Error::Remote { .. })
    ));

    revived.pipe().stop().await.expect("stop");
    while !matches!(
        revived.pipe().status().await.expect("status"),
        Status::Exited { .. }
    ) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    revived.pipe().remove().await.expect("remove");
}

/// `cargo test` sets `CARGO_MANIFEST_DIR` for this process: a variable outside the login
/// set, so it stands in for a caller's credential.
#[tokio::test]
async fn a_named_variable_reaches_the_script_and_an_unnamed_one_does_not() {
    let script = r#"printf %s "${CARGO_MANIFEST_DIR-unset}""#;
    let plain = Host::local().exec(script, b"").await.expect("exec");
    assert_eq!(plain.stdout_str(), "unset");

    let keeping = Host::local_keeping(["CARGO_MANIFEST_DIR", "NOT_SET_ANYWHERE"]);
    let out = keeping.exec(script, b"").await.expect("exec");
    assert_eq!(out.stdout_str(), env!("CARGO_MANIFEST_DIR"));
    let missing = keeping
        .exec(r#"printf %s "${NOT_SET_ANYWHERE-unset}""#, b"")
        .await
        .expect("exec");
    assert_eq!(missing.stdout_str(), "unset");

    // A stream gets the same environment as a one-shot script.
    let mut stream = keeping.open(script).await.expect("open");
    let mut seen = Vec::new();
    loop {
        match timeout(Duration::from_secs(10), stream.recv())
            .await
            .expect("recv in time")
        {
            StreamMsg::Stdout(bytes) => seen.extend(bytes),
            StreamMsg::Stderr(_) => {}
            _ => break,
        }
    }
    assert_eq!(String::from_utf8_lossy(&seen), env!("CARGO_MANIFEST_DIR"));
}
