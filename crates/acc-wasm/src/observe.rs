//! Foreign agents (TUIs ACC didn't start): discovery, observation through
//! their transcripts, and takeover into a detached session.

use acc_core::command::Command;
use acc_core::discover::{self, ForeignAgent};
use acc_core::observe::Observed;
use acc_core::session::SessionSpec;
use acc_core::takeover::{self, Takeover, TakeoverAction, TakeoverInput};
use serde_json::Value;
use wasm_bindgen::prelude::*;

use crate::session::items;
use crate::{AccExec, error, from_json, item_json, ms, to_json};

/// A `ForeignAgent` as the core serializes it, plus `key` (stable while the
/// process lives) and `pane_id` (`tmux:%3`, what `hand_back_exec` takes).
fn agent_json(a: &ForeignAgent) -> Value {
    let mut v = serde_json::to_value(a).unwrap_or_default();
    if let Some(o) = v.as_object_mut() {
        o.insert("key".into(), a.key().into());
        o.insert("pane_id".into(), a.pane.as_ref().map(takeover::pane_label).into());
    }
    v
}

fn agent_from(json: &str) -> Result<ForeignAgent, JsValue> {
    from_json(json, "agent")
}

/// Lists harness TUIs running on the host.
#[wasm_bindgen]
pub fn discover_exec() -> AccExec {
    AccExec::script(discover::discover_script())
}

/// JSON array of foreign agents; pass one back to `AccObserved` and `AccTakeover`.
#[wasm_bindgen]
pub fn parse_discover(stdout: &str) -> String {
    to_json(
        &discover::parse_discover(stdout)
            .iter()
            .map(agent_json)
            .collect::<Vec<_>>(),
    )
}

/// Why an agent can't be taken over, or `undefined` when it can.
#[wasm_bindgen]
pub fn takeover_blocker(agent_json: &str, native_id: Option<String>) -> Option<String> {
    let agent = agent_from(agent_json).ok()?;
    takeover::takeover_blocker(&agent, native_id.as_deref())
}

/// Types the resume command into the pane a session was taken from.
/// Stop the headless session first.
#[wasm_bindgen]
pub fn hand_back_exec(spec_json: &str, native_id: &str, pane: &str) -> Option<AccExec> {
    let spec = SessionSpec::from_json(spec_json)?;
    takeover::hand_back_exec(spec.harness, native_id, pane, &spec.cwd, spec.launcher.as_ref()).map(Into::into)
}

/// A shell command line that opens `harness`'s own TUI in `cwd`, for
/// palette entries whose `exec` is `terminal`. It starts a fresh session
/// rather than resuming the headless one, so the two never share a session.
#[wasm_bindgen]
pub fn tui_command(harness: &str, cwd: &str) -> Option<String> {
    let program = acc_core::multiplexer::tui::launch(crate::harness(harness).ok()?, None).ok()?;
    let argv = program.argv();
    Some(format!(
        "cd {} && exec {}",
        acc_core::shell::quote(cwd),
        acc_core::shell::join(argv.iter().map(String::as_str))
    ))
}

/// A foreign agent followed through its transcript.
#[wasm_bindgen]
#[derive(Debug)]
pub struct AccObserved {
    inner: Observed,
}

#[wasm_bindgen]
impl AccObserved {
    #[wasm_bindgen(constructor)]
    pub fn new(agent_json: &str) -> Result<AccObserved, JsValue> {
        Ok(Self {
            inner: Observed::new(agent_from(agent_json)?),
        })
    }

    pub fn agent(&self) -> String {
        to_json(&agent_json(self.inner.agent()))
    }

    /// The transcript to follow (with a fan-in), from what was read so far:
    /// `FeedEntry` JSON, or `null`.
    pub fn feed_entry(&self) -> String {
        to_json(&self.inner.feed_entry())
    }

    pub fn feed(&mut self, bytes: &[u8], now_ms: f64) -> String {
        to_json(&self.inner.feed(bytes, ms(now_ms)))
    }

    /// While a subagent runs (or one was just reported), a script that finds
    /// subagent transcripts not followed yet; call on every tick and pass
    /// its output to `on_children`. Claude and Codex keep them; others don't.
    pub fn children_exec(&mut self, now_ms: f64) -> Option<AccExec> {
        self.inner.children_exec(ms(now_ms)).map(Into::into)
    }

    /// The output of `children_exec`. True when there are new transcripts
    /// to follow: restart the fan-in with `feed_entry` and
    /// `child_feed_entries`.
    pub fn on_children(&mut self, stdout: &str) -> bool {
        self.inner.on_children(stdout)
    }

    /// The subagent transcripts to follow next to `feed_entry`.
    pub fn child_feed_entries(&self) -> String {
        to_json(&self.inner.child_feed_entries())
    }

    /// Bytes of a subagent transcript, from fan-in lines with one of the
    /// `child_feed_entries` keys. Its items carry the `subagent` item as `parent`.
    pub fn feed_child(&mut self, key: &str, bytes: &[u8], now_ms: f64) -> String {
        to_json(&self.inner.feed_child(key, bytes, ms(now_ms)))
    }

    /// The main conversation: items that didn't run inside a subagent.
    pub fn top_level_items(&self) -> String {
        items(self.inner.transcript().top_level(), self.inner.agent().harness)
    }

    /// The child conversation of the `subagent` item `parent`.
    pub fn children(&self, parent: &str) -> String {
        items(
            self.inner.transcript().children(parent),
            self.inner.agent().harness,
        )
    }

    /// Every subagent run in the session.
    pub fn subagents(&self) -> String {
        items(self.inner.transcript().subagents(), self.inner.agent().harness)
    }

    /// Call every few seconds with the output of `alive_exec` and, when
    /// `screen_exec` returned one, that capture's output.
    pub fn tick(&mut self, now_ms: f64, probe: &str, screen: Option<String>) -> String {
        to_json(&self.inner.tick(ms(now_ms), probe, screen.as_deref()))
    }

    /// A capture of the TUI's pane to pass to the next `tick`, when only the
    /// screen can tell a slow tool from an approval dialog.
    pub fn screen_exec(&self, now_ms: f64) -> Option<AccExec> {
        self.inner.screen_exec(ms(now_ms)).map(Into::into)
    }

    /// Prints `alive` or `gone`, then what the harness reports about
    /// itself. Pass the whole output to `tick`.
    pub fn alive_exec(&self) -> AccExec {
        self.inner.probe_exec().into()
    }

    pub fn is_readable(&self) -> bool {
        self.inner.is_readable()
    }

    pub fn is_replaying(&self) -> bool {
        self.inner.is_replaying()
    }

    pub fn is_alive(&self) -> bool {
        self.inner.is_alive()
    }

    pub fn at_safe_point(&self) -> bool {
        self.inner.at_safe_point()
    }

    pub fn state(&self) -> String {
        to_json(&self.inner.state())
    }

    pub fn items(&self) -> String {
        items(self.inner.transcript().items().iter(), self.inner.agent().harness)
    }

    pub fn item(&self, id: &str) -> Option<String> {
        let h = self.inner.agent().harness;
        self.inner.transcript().get(id).map(|i| to_json(&item_json(i, h)))
    }

    pub fn usage(&self) -> String {
        to_json(self.inner.usage())
    }

    pub fn quota(&self) -> String {
        to_json(&self.inner.quota())
    }

    pub fn title(&self) -> Option<String> {
        self.inner.title().map(str::to_owned)
    }

    pub fn native_id(&self) -> Option<String> {
        self.inner.native_id().map(str::to_owned)
    }

    /// Keystrokes for a `Command` JSON into the TUI's pane (prompts,
    /// interrupt, slash commands). Approvals need a takeover.
    pub fn pane_command(&self, command_json: &str) -> Result<Vec<AccExec>, JsValue> {
        let command: Command = from_json(command_json, "command")?;
        self.inner
            .pane_command(&command)
            .map(|execs| execs.into_iter().map(Into::into).collect())
            .map_err(|e| error("Unsupported", &e.to_string()))
    }
}

/// What a takeover's client should do next. `kind` is `run` (run `exec`),
/// `wait` (`ms`), `launch` (the TUI is gone: launch `request`, a
/// `LaunchRequest` JSON, as a new session) or `failed` (`reason`). `step`
/// says where the takeover is, for progress displays.
#[wasm_bindgen(getter_with_clone)]
#[derive(Debug, Clone, PartialEq)]
pub struct AccTakeoverAction {
    pub kind: String,
    pub step: Option<String>,
    pub exec: Option<AccExec>,
    pub ms: Option<f64>,
    pub request: Option<String>,
    pub reason: Option<String>,
}

impl From<TakeoverAction> for AccTakeoverAction {
    fn from(a: TakeoverAction) -> Self {
        let mut out = Self {
            kind: String::new(),
            step: None,
            exec: None,
            ms: None,
            request: None,
            reason: None,
        };
        let step = |s| Some(to_json(&s).trim_matches('"').to_owned());
        match a {
            TakeoverAction::Run { exec, step: s } => {
                out.kind = "run".into();
                out.exec = Some(exec.into());
                out.step = step(s);
            }
            TakeoverAction::Wait { ms, step: s } => {
                out.kind = "wait".into();
                out.ms = Some(crate::num(ms));
                out.step = step(s);
            }
            TakeoverAction::Launch { request } => {
                out.kind = "launch".into();
                out.request = Some(to_json(&request));
            }
            TakeoverAction::Failed { reason } => {
                out.kind = "failed".into();
                out.reason = Some(reason);
            }
        }
        out
    }
}

/// Closes a foreign TUI safely and hands its conversation to a detached
/// session. Step it with what you observe; run what it returns.
#[wasm_bindgen]
#[derive(Debug)]
pub struct AccTakeover {
    inner: Takeover,
}

#[wasm_bindgen]
impl AccTakeover {
    #[wasm_bindgen(constructor)]
    pub fn new(agent_json: &str, native_id: Option<String>, force: bool) -> Result<AccTakeover, JsValue> {
        let t = Takeover::new(agent_from(agent_json)?, native_id, force)
            .map_err(|message| error("Unsupported", &message))?;
        Ok(Self { inner: t })
    }

    /// Steps with a `TakeoverInput` JSON: `{"type":"tick","now_ms":…,"safe":…}`
    /// or `{"type":"ran","now_ms":…,"stdout":"…"}`.
    pub fn next(&mut self, input_json: &str) -> Result<AccTakeoverAction, JsValue> {
        let input: TakeoverInput = from_json(input_json, "takeover input")?;
        Ok(self.inner.next(input).into())
    }
}
