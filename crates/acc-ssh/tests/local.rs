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
