//! OSC 7501 "Program Status Protocol" parsing.
//!
//! ```text
//! OSC 7501 ; pairs ST        report
//! OSC 7501 ; ? ST            feature probe
//! pairs := key=value (":" key=value)*      ST = ESC \  or  BEL
//! ```
//!
//! Unknown keys are ignored and malformed reports are discarded, per the spec.
//! Spec: <https://www.superlogical.com/rex/docs/build/program-status>.

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::state::{BlockKind, RunState};

/// One parsed OSC 7501 message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Osc7501 {
    /// `OSC 7501 ; ?`: the program asks whether the terminal supports reports.
    Probe,
    Report(Report),
}

/// A status report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// `None` for `state=clear`.
    pub state: Option<RunState>,
    /// Child record path such as `agents/3`; `None` is the root record.
    pub id: Option<String>,
    pub app: Option<String>,
    pub title: Option<String>,
    pub message: Option<String>,
}

/// The reply a terminal sends to a probe to say it supports OSC 7501.
pub const PROBE_REPLY: &[u8] = b"\x1b]7501;?\x1b\\";

/// Parses the payload after `7501;` (without the terminator).
pub fn parse_payload(payload: &str) -> Option<Osc7501> {
    if payload == "?" {
        return Some(Osc7501::Probe);
    }
    let mut state = None;
    let mut kind = None;
    let mut progress = None;
    let mut report = Report {
        state: None,
        id: None,
        app: None,
        title: None,
        message: None,
    };
    for pair in payload.split(':') {
        let (key, value) = pair.split_once('=')?;
        match key {
            "state" => state = Some(value),
            "kind" => kind = Some(value),
            "progress" => progress = value.parse::<u8>().ok().filter(|p| *p <= 100),
            "id" => report.id = Some(value.to_owned()).filter(|v| !v.is_empty()),
            "app" => report.app = Some(value.to_owned()),
            "title" => report.title = decode_b64(value),
            "msg" => report.message = decode_b64(value),
            _ => {}
        }
    }
    report.state = match state? {
        "idle" => Some(RunState::Idle),
        "working" => Some(RunState::Working { progress }),
        "done" => Some(RunState::Done),
        "error" => Some(RunState::Error),
        "blocked" => Some(RunState::Blocked {
            kind: match kind {
                Some("question") => BlockKind::Question,
                Some("auth") => BlockKind::Auth,
                _ => BlockKind::Permission,
            },
        }),
        "clear" => None,
        _ => return None,
    };
    Some(Osc7501::Report(report))
}

fn decode_b64(value: &str) -> Option<String> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(value).ok()?;
    String::from_utf8(bytes).ok()
}

/// Extracts OSC 7501 messages from a raw terminal byte stream, across chunk
/// boundaries. Other bytes are ignored; a VT parser renders them separately.
#[derive(Debug, Default)]
pub struct Scanner {
    state: ScanState,
    buf: Vec<u8>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum ScanState {
    #[default]
    Ground,
    Esc,
    Osc,
    OscEsc,
}

/// OSC payloads longer than this are dropped (reports are one short line).
const MAX_OSC: usize = 4096;

impl Scanner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Osc7501> {
        let mut out = Vec::new();
        for &b in bytes {
            self.state = match (self.state, b) {
                (ScanState::Ground, 0x1b) => ScanState::Esc,
                (ScanState::Ground, _) => ScanState::Ground,
                (ScanState::Esc, b']') => {
                    self.buf.clear();
                    ScanState::Osc
                }
                (ScanState::Esc, 0x1b) => ScanState::Esc,
                (ScanState::Esc, _) => ScanState::Ground,
                (ScanState::Osc, 0x07) => {
                    self.finish(&mut out);
                    ScanState::Ground
                }
                (ScanState::Osc, 0x1b) => ScanState::OscEsc,
                (ScanState::Osc, _) => {
                    if self.buf.len() < MAX_OSC {
                        self.buf.push(b);
                        ScanState::Osc
                    } else {
                        self.buf.clear();
                        ScanState::Ground
                    }
                }
                (ScanState::OscEsc, b'\\') => {
                    self.finish(&mut out);
                    ScanState::Ground
                }
                // ESC inside an OSC that isn't ST aborts the OSC and starts a new escape.
                (ScanState::OscEsc, b']') => {
                    self.buf.clear();
                    ScanState::Osc
                }
                (ScanState::OscEsc, _) => ScanState::Ground,
            };
        }
        out
    }

    fn finish(&mut self, out: &mut Vec<Osc7501>) {
        if let Some(rest) = self.buf.strip_prefix(b"7501;")
            && let Ok(payload) = std::str::from_utf8(rest)
            && let Some(msg) = parse_payload(payload)
        {
            out.push(msg);
        }
        self.buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_blocked_permission_with_message() {
        // "Edit main.rs" in base64.
        let msg = parse_payload("state=blocked:kind=permission:app=claude-code:msg=RWRpdCBtYWluLnJz");
        let Some(Osc7501::Report(r)) = msg else {
            unreachable!("expected report")
        };
        assert_eq!(
            r.state,
            Some(RunState::Blocked {
                kind: BlockKind::Permission
            })
        );
        assert_eq!(r.app.as_deref(), Some("claude-code"));
        assert_eq!(r.message.as_deref(), Some("Edit main.rs"));
    }

    #[test]
    fn progress_and_child_id() {
        let Some(Osc7501::Report(r)) = parse_payload("state=working:progress=42:id=agents/1") else {
            unreachable!("expected report")
        };
        assert_eq!(r.state, Some(RunState::Working { progress: Some(42) }));
        assert_eq!(r.id.as_deref(), Some("agents/1"));
    }

    #[test]
    fn rejects_malformed_and_unknown_states() {
        assert_eq!(parse_payload("state=sleeping"), None);
        assert_eq!(parse_payload("nonsense"), None);
        assert_eq!(parse_payload("app=x"), None);
    }

    #[test]
    fn ignores_unknown_keys() {
        assert!(matches!(
            parse_payload("state=idle:future=1"),
            Some(Osc7501::Report(_))
        ));
    }

    #[test]
    fn scanner_handles_split_chunks_and_both_terminators() {
        let mut s = Scanner::new();
        let mut got = s.feed(b"hello \x1b]7501;sta");
        got.extend(s.feed(b"te=done\x07 more \x1b]0;title\x07"));
        got.extend(s.feed(b"\x1b]7501;?\x1b\\"));
        assert_eq!(got.len(), 2);
        assert!(matches!(&got[0], Osc7501::Report(r) if r.state == Some(RunState::Done)));
        assert_eq!(got[1], Osc7501::Probe);
    }

    #[test]
    fn scanner_drops_oversized_osc() {
        let mut s = Scanner::new();
        let mut big = b"\x1b]7501;state=idle:x=".to_vec();
        big.extend(std::iter::repeat_n(b'a', MAX_OSC + 10));
        big.push(0x07);
        assert!(s.feed(&big).is_empty());
        assert_eq!(s.feed(b"\x1b]7501;state=idle\x07").len(), 1);
    }
}
