//! `AccSession`, `AccFanIn` and the host scripts, as UniFFI objects and
//! functions.
//!
//! Everything is a byte pump: the app runs the [`AccExec`]s it is given over
//! its own SSH channels and feeds back what they print. Nothing here calls
//! back into the app, so objects can live inside a Swift actor and be used
//! from one task without any cross-thread callbacks.

use std::sync::{Arc, Mutex, MutexGuard};

use acc_core::detached::{self, FanInParser, FeedEntry};
use acc_core::harness;
use acc_core::item;
use acc_core::session::{self, Session, SessionError, SessionSpec};

use crate::AccError;
use crate::acc_types::*;

impl From<SessionError> for AccError {
    fn from(e: SessionError) -> Self {
        match e {
            SessionError::Driver(e) => e.into(),
            e => Self::InvalidSession {
                reason: e.to_string(),
            },
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// One detached agent session on a host, from this client's point of view.
///
/// 1. `launch` (then run `take_launch_exec`; its stdout is the PID) or
///    `attach` (then run `status_exec` and pass its stdout to `on_status`).
/// 2. Follow the log from `offset` and pass every chunk to `feed`.
/// 3. After `feed` and `command`, send `take_transmit` with `input_exec`.
#[derive(Debug, uniffi::Object)]
pub struct AccSession {
    inner: Mutex<Session>,
    launch: Mutex<Option<AccExec>>,
}

#[uniffi::export]
impl AccSession {
    /// Starts a new session. `id` comes from `new_session_id`; `client_tag`
    /// must be unique among clients that may share the session.
    #[uniffi::constructor]
    pub fn launch(
        request: AccLaunchRequest,
        id: String,
        client_tag: String,
        now_ms: u64,
    ) -> Result<Arc<Self>, AccError> {
        let (session, exec) = Session::launch(request.into(), &id, &client_tag, now_ms)?;
        Ok(Arc::new(Self {
            inner: Mutex::new(session),
            launch: Mutex::new(Some(exec.into())),
        }))
    }

    /// Joins a session from its `spec.json` (an `AccSessionSpec.json`).
    #[uniffi::constructor]
    pub fn attach(spec_json: String, client_tag: String) -> Result<Arc<Self>, AccError> {
        let spec = SessionSpec::from_json(&spec_json).ok_or_else(|| AccError::InvalidSession {
            reason: "unreadable spec.json".into(),
        })?;
        Ok(Arc::new(Self {
            inner: Mutex::new(Session::attach(spec, &client_tag)?),
            launch: Mutex::new(None),
        }))
    }

    /// Picks a session up from `snapshot` (see `snapshot`), for the session
    /// in `spec_json` as the host lists it now. Continue as after `attach`.
    /// Fails for a snapshot of another session or ACC version: attach then.
    #[uniffi::constructor]
    pub fn restore(snapshot: String, spec_json: String) -> Result<Arc<Self>, AccError> {
        let spec = SessionSpec::from_json(&spec_json).ok_or_else(|| AccError::InvalidSession {
            reason: "unreadable spec.json".into(),
        })?;
        Ok(Arc::new(Self {
            inner: Mutex::new(Session::restore(&snapshot, spec)?),
            launch: Mutex::new(None),
        }))
    }

    /// Everything read of the session so far, to persist and `restore`
    /// later instead of replaying the whole log.
    pub fn snapshot(&self) -> Option<String> {
        lock(&self.inner).snapshot()
    }

    /// The script that starts the harness, once, for a launched session.
    pub fn take_launch_exec(&self) -> Option<AccExec> {
        lock(&self.launch).take()
    }

    pub fn status_exec(&self) -> AccExec {
        lock(&self.inner).status_exec().into()
    }

    /// Follows the log from the current offset on a channel of its own.
    pub fn attach_exec(&self) -> AccExec {
        lock(&self.inner).attach_exec().into()
    }

    pub fn input_exec(&self, bytes: Vec<u8>) -> AccExec {
        lock(&self.inner).input_exec(bytes).into()
    }

    pub fn kill_exec(&self) -> AccExec {
        lock(&self.inner).kill_exec().into()
    }

    pub fn remove_exec(&self) -> AccExec {
        lock(&self.inner).remove_exec().into()
    }

    /// A rewrite of `spec.json`, when the native id or title became known.
    pub fn take_spec_write(&self) -> Option<AccExec> {
        lock(&self.inner).take_spec_write().map(Into::into)
    }

    pub fn on_status(&self, stdout: String, now_ms: u64) -> Vec<AccUpdate> {
        convert(lock(&self.inner).on_status(&stdout, now_ms))
    }

    /// The content of the session's exit file, from a fan-in `SessionExit` entry.
    pub fn on_exit_file(&self, text: String, now_ms: u64) -> Vec<AccUpdate> {
        convert(lock(&self.inner).on_exit_file(&text, now_ms))
    }

    /// Bytes of the log, starting at `offset`.
    pub fn feed(&self, bytes: Vec<u8>, now_ms: u64) -> Vec<AccUpdate> {
        convert(lock(&self.inner).feed(&bytes, now_ms))
    }

    pub fn command(&self, command: AccCommand) -> Result<AccOutbound, AccError> {
        Ok(lock(&self.inner).command(command.into())?.into())
    }

    /// What the last command did that the harness won't report (the prompt
    /// of an ACP agent). Call after `command`; otherwise `feed` returns it.
    pub fn take_updates(&self, now_ms: u64) -> Vec<AccUpdate> {
        convert(lock(&self.inner).take_updates(now_ms))
    }

    /// Bytes to write to the harness now (send with `input_exec`).
    pub fn take_transmit(&self) -> Vec<u8> {
        lock(&self.inner).take_transmit()
    }

    pub fn spec(&self) -> AccSessionSpec {
        lock(&self.inner).spec().into()
    }

    pub fn offset(&self) -> u64 {
        lock(&self.inner).offset()
    }

    pub fn is_replaying(&self) -> bool {
        lock(&self.inner).is_replaying()
    }

    pub fn is_alive(&self) -> bool {
        lock(&self.inner).is_alive()
    }

    pub fn state(&self) -> Option<AccSessionState> {
        lock(&self.inner).state().cloned().map(Into::into)
    }

    pub fn items(&self) -> Vec<AccItem> {
        let s = lock(&self.inner);
        let h = s.harness();
        s.transcript().items().iter().map(|i| item(i, h)).collect()
    }

    pub fn item(&self, id: String) -> Option<AccItem> {
        let s = lock(&self.inner);
        let h = s.harness();
        s.transcript().get(&id).map(|i| item(i, h))
    }

    /// The main conversation: items that didn't run inside a subagent.
    pub fn top_level_items(&self) -> Vec<AccItem> {
        let s = lock(&self.inner);
        let h = s.harness();
        s.transcript().top_level().map(|i| item(i, h)).collect()
    }

    /// The child conversation of the `Subagent` item `parent`.
    pub fn children(&self, parent: String) -> Vec<AccItem> {
        let s = lock(&self.inner);
        let h = s.harness();
        s.transcript().children(&parent).map(|i| item(i, h)).collect()
    }

    /// Every subagent run in the session.
    pub fn subagents(&self) -> Vec<AccItem> {
        let s = lock(&self.inner);
        let h = s.harness();
        s.transcript().subagents().map(|i| item(i, h)).collect()
    }

    pub fn usage(&self) -> AccUsage {
        lock(&self.inner).usage().clone().into()
    }

    pub fn quota(&self) -> Option<AccQuota> {
        lock(&self.inner).quota().cloned().map(Into::into)
    }

    pub fn permissions(&self) -> Vec<AccPermission> {
        lock(&self.inner)
            .permissions()
            .iter()
            .cloned()
            .map(Into::into)
            .collect()
    }

    pub fn commands(&self) -> Vec<AccCommandEntry> {
        lock(&self.inner).commands().iter().map(Into::into).collect()
    }

    pub fn options(&self) -> Vec<AccOptionDef> {
        lock(&self.inner).options().iter().map(Into::into).collect()
    }
}

fn convert(updates: Vec<acc_core::session::Update>) -> Vec<AccUpdate> {
    updates.into_iter().map(Into::into).collect()
}

/// Follows many files on a host over one channel (see `script`).
#[derive(Debug, uniffi::Object)]
pub struct AccFanIn {
    entries: Vec<FeedEntry>,
    parser: Mutex<FanInParser>,
}

#[uniffi::export]
impl AccFanIn {
    #[uniffi::constructor]
    pub fn new(entries: Vec<AccFeedEntry>) -> Arc<Self> {
        let entries: Vec<FeedEntry> = entries.into_iter().map(Into::into).collect();
        Arc::new(Self {
            parser: Mutex::new(FanInParser::new(&entries)),
            entries,
        })
    }

    /// The long-running script; run it on a long-lived channel.
    pub fn exec(&self) -> AccExec {
        AccExec::script(detached::fan_in_script(&self.entries))
    }

    pub fn feed(&self, bytes: Vec<u8>) -> Vec<AccFeedLine> {
        lock(&self.parser)
            .feed(&bytes)
            .into_iter()
            .map(|l| AccFeedLine {
                key: l.key,
                bytes: l.bytes,
                offset: l.offset,
            })
            .collect()
    }

    /// Bytes of `key`'s file consumed so far.
    pub fn offset(&self, key: String) -> Option<u64> {
        lock(&self.parser).offset(&key)
    }
}

/// Lists every detached session on the host.
#[uniffi::export]
pub fn list_sessions_exec() -> AccExec {
    AccExec::script(detached::list_script())
}

#[uniffi::export]
pub fn parse_session_list(stdout: String) -> Vec<AccListing> {
    detached::parse_list(&stdout)
        .into_iter()
        .map(Into::into)
        .collect()
}

#[uniffi::export]
pub fn parse_session_status(stdout: String) -> Option<AccStatus> {
    detached::parse_status(&stdout).map(Into::into)
}

/// Finds the installed harnesses (login `PATH`, with versions).
#[uniffi::export]
pub fn probe_exec() -> AccExec {
    AccExec::script(harness::probe_script())
}

#[uniffi::export]
pub fn parse_probe(stdout: String) -> Vec<AccAvailability> {
    harness::parse_probe(&stdout)
        .into_iter()
        .map(Into::into)
        .collect()
}

/// Prints the absolute path of a directory (`~` expanded) or fails.
#[uniffi::export]
pub fn resolve_dir_exec(dir: String) -> AccExec {
    AccExec::script(session::resolve_dir_script(&dir))
}

/// A session id from at least 15 random bytes supplied by the caller.
#[uniffi::export]
pub fn new_session_id(harness: AccHarness, entropy: Vec<u8>) -> String {
    session::new_session_id(harness.into(), &entropy)
}

/// Parses `git diff` output into files and hunks.
#[uniffi::export]
pub fn parse_unified_diff(text: String) -> Vec<AccFileDiff> {
    item::parse_unified(&text, "")
        .into_iter()
        .map(Into::into)
        .collect()
}

/// Display name of a harness.
#[uniffi::export]
pub fn harness_display_name(harness: AccHarness) -> String {
    acc_core::Harness::from(harness).display_name().to_owned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const HI: &[u8] = include_bytes!("../../acc-core/tests/fixtures/claude/hi-and-controls.ndjson");

    #[test]
    fn launch_attach_feed_round_trip_through_ffi_types() {
        let req = AccLaunchRequest {
            harness: AccHarness::Claude,
            cwd: "/w".into(),
            model: None,
            permission: None,
            resume: None,
            fork: false,
            title: None,
            prompt: Some("hi".into()),
            attachments: vec![],
            parent: None,
            replaces_parent: false,
            origin: AccOrigin::Started,
            launcher: None,
        };
        let id = new_session_id(AccHarness::Claude, vec![7; 15]);
        let s = AccSession::launch(req, id, "ph".into(), 1).unwrap();
        assert!(s.take_launch_exec().is_some());
        assert!(s.take_launch_exec().is_none());
        assert!(!s.take_transmit().is_empty());

        let b = AccSession::attach(s.spec().json, "desk".into()).unwrap();
        b.on_status(format!("alive 3 {}", HI.len()), 2);
        let ups = b.feed(HI.to_vec(), 3);
        assert!(ups.iter().all(|u| u.replay));
        assert!(
            b.items()
                .iter()
                .any(|i| i.kind == AccItemKind::AgentMessage && i.text == "hi")
        );
        assert!(matches!(
            b.command(AccCommand::Fork).unwrap(),
            AccOutbound::Relaunch { .. }
        ));
        assert!(b.options().iter().any(|o| o.id == AccOptionId::Model));
    }

    #[test]
    fn images_cross_as_bytes_and_oversized_ones_are_rejected() {
        let req = AccLaunchRequest {
            harness: AccHarness::Claude,
            cwd: "/w".into(),
            model: None,
            permission: None,
            resume: None,
            fork: false,
            title: None,
            prompt: None,
            attachments: vec![],
            parent: None,
            replaces_parent: false,
            origin: AccOrigin::Started,
            launcher: None,
        };
        let id = new_session_id(AccHarness::Claude, vec![8; 15]);
        let s = AccSession::launch(req, id, "ph".into(), 1).unwrap();
        s.take_transmit();
        let png = AccAttachment {
            media_type: "image/png".into(),
            data: b"PNG".to_vec(),
        };
        let AccOutbound::Send { bytes } = s
            .command(AccCommand::Prompt {
                text: "look".into(),
                attachments: vec![png],
            })
            .unwrap()
        else {
            panic!("prompts are sent, not relaunched");
        };
        let line = String::from_utf8(bytes).unwrap();
        assert!(line.contains("\"type\":\"image\"") && line.contains("\"data\":\"UE5H\""));

        let huge = AccAttachment {
            media_type: "image/png".into(),
            data: vec![0; 5 * 1024 * 1024 + 1],
        };
        assert!(matches!(
            s.command(AccCommand::Prompt {
                text: String::new(),
                attachments: vec![huge],
            }),
            Err(AccError::AttachmentRejected { .. })
        ));
    }

    #[test]
    fn subagents_cross_the_ffi() {
        const SUB: &[u8] = include_bytes!("../../acc-core/tests/fixtures/claude/subagents.ndjson");
        let req = AccLaunchRequest {
            harness: AccHarness::Claude,
            cwd: "/w".into(),
            model: None,
            permission: None,
            resume: None,
            fork: false,
            title: None,
            prompt: None,
            attachments: vec![],
            parent: None,
            replaces_parent: false,
            origin: AccOrigin::Started,
            launcher: None,
        };
        let s = AccSession::launch(
            req,
            new_session_id(AccHarness::Claude, vec![9; 15]),
            "ph".into(),
            1,
        )
        .unwrap();
        let b = AccSession::attach(s.spec().json, "desk".into()).unwrap();
        b.on_status(format!("exited 0 {}", SUB.len()), 2);
        let ups = b.feed(SUB.to_vec(), 3);
        assert!(
            ups.iter()
                .any(|u| matches!(&u.event, AccEvent::ItemStarted { parent: Some(_), .. }))
        );
        let subs = b.subagents();
        assert_eq!(subs.len(), 2);
        assert!(subs.iter().all(|i| i.kind == AccItemKind::Subagent));
        let AccItemDetail::Subagent { agent, child, .. } = &subs[0].detail else {
            panic!("subagent detail")
        };
        assert_eq!(agent.as_deref(), Some("Explore"));
        assert!(child.is_some());
        assert!(!b.children(subs[0].id.clone()).is_empty());
        assert!(b.top_level_items().iter().all(|i| i.parent.is_none()));
    }

    #[test]
    fn snapshots_restore_through_ffi() {
        let s = AccSession::launch(
            AccLaunchRequest {
                harness: AccHarness::Claude,
                cwd: "/w".into(),
                model: None,
                permission: None,
                resume: None,
                fork: false,
                title: None,
                prompt: None,
                attachments: vec![],
                parent: None,
                replaces_parent: false,
                origin: AccOrigin::Started,
                launcher: None,
            },
            new_session_id(AccHarness::Claude, vec![10; 15]),
            "ph".into(),
            1,
        )
        .unwrap();
        let spec = s.spec().json;
        let a = AccSession::attach(spec.clone(), "desk".into()).unwrap();
        a.on_status(format!("alive 3 {}", HI.len()), 2);
        a.feed(HI.to_vec(), 3);
        let b = AccSession::restore(a.snapshot().unwrap(), spec.clone()).unwrap();
        assert_eq!(b.offset(), HI.len() as u64);
        assert_eq!(b.items(), a.items());
        assert!(matches!(
            AccSession::restore("{}".into(), spec),
            Err(AccError::InvalidSession { .. })
        ));
    }

    #[test]
    fn fan_in_and_listing() {
        let f = AccFanIn::new(vec![AccFeedEntry {
            key: "k".into(),
            path: AccFeedPath::SessionLog { id: "abc".into() },
            offset: 4,
        }]);
        assert!(f.exec().script.contains("tail -c +\"$3\""));
        let lines = f.feed(b"k\t{}\n".to_vec());
        assert_eq!(lines[0].offset, 7);
        let list = parse_session_list("S\tabc\talive 1 2\t-\n".into());
        assert_eq!(list[0].status, AccStatus::Alive { pid: 1, size: 2 });
    }
}
