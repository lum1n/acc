//! `Host::local`: the same detached sessions on this machine, with `sh` instead of SSH. Sessions
//! live under the current user's `~/.acc/s` with unique ids and are removed afterwards.

#![cfg(all(unix, feature = "local"))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use acc_core::harness::{LaunchSpec, Protocol};
use acc_ssh::{Detached, Host, Read, Status, list};
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
