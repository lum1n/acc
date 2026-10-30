//! The SSH surface of the bindings, against a real OpenSSH server on
//! localhost, called exactly as Swift and Kotlin call it.
//!
//! Skips when `sshd` is missing, unless `ACC_REQUIRE_SSHD=1` (set in CI).

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

#[path = "../../acc-ssh/tests/support/sshd.rs"]
mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use acc_core::harness::{LaunchSpec as CoreSpec, Protocol};
use acc_ffi::*;
use tokio::time::timeout;

fn config(sshd: &support::Sshd, host_key: HostKeyCheck) -> SshConfig {
    SshConfig {
        host: "127.0.0.1".into(),
        port: sshd.port,
        user: support::user(),
        auth: SshAuth::PrivateKey {
            key: sshd.key.clone(),
            passphrase: None,
        },
        host_key,
        connect_timeout_ms: Some(5_000),
        keepalive_interval_ms: None,
        keepalive_max: None,
    }
}

fn session_config(harness: Harness) -> SessionConfig {
    SessionConfig {
        sandbox: None,
        harness,
        cwd: "/tmp".into(),
        opening: Opening::New,
        model: None,
        client_tag: None,
    }
}

async fn events_until(
    next: impl AsyncFn() -> Result<Vec<Event>, AccError>,
    stop: impl Fn(&Event) -> bool,
) -> Vec<Event> {
    let mut seen = Vec::new();
    loop {
        let got = timeout(Duration::from_secs(20), next())
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
async fn host_errors_and_exec() {
    let Some(sshd) = support::start_sshd("ffi-host") else {
        return;
    };
    let host = SshHost::new(config(
        &sshd,
        HostKeyCheck::Fingerprints {
            fingerprints: vec!["SHA256:nope".into()],
        },
    ))
    .unwrap();
    match host.connect().await {
        Err(AccError::HostKeyRejected { fingerprint }) => assert_eq!(fingerprint, sshd.fingerprint),
        other => panic!("expected a host key error, got {other:?}"),
    }

    // Trust on first use: connect, read the fingerprint, pin it.
    let tofu = SshHost::new(config(&sshd, HostKeyCheck::AcceptAny)).unwrap();
    tofu.connect().await.unwrap();
    assert_eq!(tofu.host_key_fingerprint(), Some(sshd.fingerprint.clone()));
    let out = tofu.exec("cat; exit 3".into(), b"in".to_vec()).await.unwrap();
    assert_eq!((out.stdout, out.exit_code), (b"in".to_vec(), Some(3)));

    let mut bad = config(&sshd, HostKeyCheck::AcceptAny);
    bad.auth = SshAuth::Password {
        password: "wrong".into(),
    };
    match SshHost::new(bad).unwrap().connect().await {
        Err(AccError::AuthRejected { .. } | AccError::Ssh { .. }) => {}
        other => panic!("expected a login error, got {other:?}"),
    }
    let mut bad = config(&sshd, HostKeyCheck::AcceptAny);
    bad.auth = SshAuth::PrivateKey {
        key: "not a key".into(),
        passphrase: None,
    };
    assert!(matches!(SshHost::new(bad), Err(AccError::InvalidArgument { .. })));
    let launched = tofu
        .launch_session("../x".into(), session_config(Harness::Claude))
        .await;
    assert!(
        matches!(launched, Err(AccError::InvalidArgument { .. })),
        "{launched:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attaches_to_a_detached_session() {
    let Some(sshd) = support::start_sshd("ffi-session") else {
        return;
    };
    let check = HostKeyCheck::Fingerprints {
        fingerprints: vec![sshd.fingerprint.clone()],
    };
    let host = SshHost::new(config(&sshd, check.clone())).unwrap();

    // Another client (the desktop) started a session; it waits on stdin.
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../acc-core/tests/fixtures/claude/permission-deny.ndjson");
    let inlog = sshd.dir.join("stdin");
    let spec = CoreSpec {
        sandbox: None,
        program: "sh".into(),
        args: vec![
            "-c".into(),
            r#"cat "$1"; exec cat >> "$2""#.into(),
            "sh".into(),
            fixture.display().to_string(),
            inlog.display().to_string(),
        ],
        env: vec![],
        protocol: Protocol::ClaudeStreamJson,
    };
    let id = format!("t-ffi-{}", std::process::id());
    let ssh_host = Arc::new(acc_ssh::Host::new({
        let mut c = acc_ssh::SshConfig::new(
            "127.0.0.1",
            support::user(),
            acc_ssh::Auth::key(&sshd.key, None).unwrap(),
            acc_ssh::HostKeyCheck::AcceptAny,
        );
        c.port = sshd.port;
        c
    }));
    let record = acc_ssh::SessionSpec::new(&id, acc_core::Harness::Claude, "/tmp", spec.clone(), 0).to_json();
    acc_ssh::Detached::launch(ssh_host, &id, &spec, "/tmp", Some(&record))
        .await
        .unwrap();

    let listed = host.list_sessions().await.unwrap();
    let info = listed.iter().find(|l| l.id == id).expect("listed");
    assert!(matches!(info.status, DetachedStatus::Alive { .. }));
    assert_eq!(info.spec.as_ref().map(|s| s.program.as_str()), Some("sh"));

    // The phone joins it and replays the session.
    let s = host
        .attach_session(id.clone(), session_config(Harness::Claude))
        .unwrap();
    assert_eq!((s.id(), s.harness()), (id.clone(), Harness::Claude));
    let seen = events_until(
        async || s.next_events().await,
        |e| matches!(e, Event::TurnCompleted),
    )
    .await;
    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::PermissionRequested { .. }))
    );
    assert_eq!(
        s.native_id().as_deref(),
        Some("5f3c1a52-8a3e-4c1e-9d47-2b6f0e9a7c11")
    );
    assert!(!s.commands().is_empty());
    assert!(s.offset() > 0);
    assert_eq!(s.state().map(|st| st.source), Some(StateSource::Native));

    s.send(Command::Prompt {
        text: "from the phone".into(),
        attachments: vec![],
    })
    .await
    .unwrap();
    s.stop().await.unwrap();
    let exit = events_until(
        async || s.next_events().await,
        |e| matches!(e, Event::Exited { .. }),
    )
    .await;
    assert!(
        exit.iter()
            .any(|e| matches!(e, Event::StateChanged { state } if state.state == ProgramState::Exited))
    );
    assert!(matches!(s.status().await.unwrap(), DetachedStatus::Exited { .. }));
    assert!(matches!(
        s.send(Command::Prompt {
            text: "late".into(),
            attachments: vec![],
        })
        .await,
        Err(AccError::NotRunning { .. })
    ));
    assert!(
        std::fs::read_to_string(&inlog)
            .unwrap()
            .contains("from the phone")
    );
    host.remove_session(id.clone()).await.unwrap();
    assert!(!host.list_sessions().await.unwrap().iter().any(|l| l.id == id));
}
