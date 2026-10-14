//! The wasm surface driven natively, the way a JS client would.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use acc_wasm::*;
use serde_json::Value;

const HI: &[u8] = include_bytes!("../../acc-core/tests/fixtures/claude/hi-and-controls.ndjson");

fn json(s: &str) -> Value {
    serde_json::from_str(s).unwrap()
}

#[test]
fn launch_attach_feed_round_trip_through_json() {
    let id = new_session_id("claude", &[7; 15]).unwrap();
    let mut s = AccSession::launch(
        r#"{"harness":"claude","cwd":"/w","prompt":"hi"}"#,
        &id,
        "web",
        1.0,
    )
    .unwrap();
    assert!(s.take_launch_exec().is_some_and(|e| e.script.contains("nohup")));
    assert!(s.take_launch_exec().is_none());
    assert!(!s.take_transmit().is_empty());

    let mut b = AccSession::attach(&s.spec(), "desk").unwrap();
    b.on_status(&format!("alive 3 {}", HI.len()), 2.0);
    let ups = json(&b.feed(HI, 3.0));
    assert!(ups.as_array().unwrap().iter().all(|u| u["replay"] == true));
    assert_eq!(b.offset(), HI.len() as f64);
    let items = json(&b.items());
    assert!(
        items
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["kind"] == "agent_message" && i["text"] == "hi" && i["detail"].is_object())
    );
    let fork = b.command(r#"{"type":"fork"}"#).unwrap();
    assert_eq!(fork.kind, "relaunch");
    assert!(fork.request.is_some_and(|r| json(&r)["fork"] == true));
    let prompt = b.command(r#"{"type":"prompt","text":"again"}"#).unwrap();
    assert_eq!(prompt.kind, "send");
    assert!(String::from_utf8(prompt.bytes).unwrap().contains("again"));
    assert!(
        json(&b.options())
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["id"] == "model")
    );

    let restored = AccSession::restore(&b.snapshot().unwrap(), &b.spec()).unwrap();
    assert_eq!(restored.items(), b.items());
}

#[test]
fn fan_in_lines_carry_bytes_and_offsets() {
    let mut f =
        AccFanIn::new(r#"[{"key":"k","path":{"type":"session_log","value":"abc"},"offset":4}]"#).unwrap();
    assert!(f.exec().script.contains("tail -c +\"$3\""));
    let lines = f.feed(b"k\t{}\n");
    assert_eq!(
        (lines[0].key.as_str(), lines[0].bytes.as_slice(), lines[0].offset),
        ("k", &b"{}\n"[..], 7.0)
    );
    assert_eq!(f.offset("k"), Some(7.0));
    let list = json(&parse_session_list("S\tabc\talive 1 2\t-\n"));
    assert_eq!(list[0]["id"], "abc");
}

#[test]
fn probe_reports_compat_and_harnesses_list_every_harness() {
    let found = json(&parse_probe("H\tclaude\t/u/bin/claude\t2.1.296 (Claude Code)\n"));
    assert_eq!(found[0]["compat"], "tested");
    let all = json(&harnesses());
    assert_eq!(all.as_array().unwrap().len(), 6);
    assert_eq!(
        harness_display_name("claude-code").unwrap(),
        harness_display_name("claude").unwrap()
    );
}
