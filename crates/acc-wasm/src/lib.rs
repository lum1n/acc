//! wasm-bindgen surface of acc-core for browsers and Node.
//!
//! The same objects and functions as acc-ffi, under the same names, so every
//! embedder gets one API (`tests/parity.rs` keeps the two in step). Records
//! cross as JSON strings shaped exactly like the core's serde types; the ones
//! that carry bytes ([`AccExec`], [`AccOutbound`], [`AccFeedLine`]) are
//! classes whose byte fields are `Uint8Array`s, so prompts with images and
//! log chunks never turn into JSON number arrays. Times and offsets are plain
//! JS numbers (milliseconds, bytes).
//!
//! Errors are thrown as `Error`s named `AccError` with a `kind` property:
//! `InvalidSession`, `Unsupported`, `UnknownRequest`, `InvalidOption`,
//! `NotStarted`, `MissingArgument`, `AttachmentRejected` (as acc-ffi's
//! `AccError`), or `InvalidJson` for an argument that didn't parse.

pub mod observe;
pub mod session;

pub use observe::*;
pub use session::*;

use acc_core::osc7501;
use acc_core::transcript::Item;
use acc_core::{Exec, Harness};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use wasm_bindgen::prelude::*;

/// Version of the core library.
#[wasm_bindgen]
pub fn acc_version() -> String {
    acc_core::VERSION.to_owned()
}

/// JSON array of the harnesses ACC supports: `harness` (the name every
/// other record uses), `id`, `program`, `display_name`, `native_acp`.
#[wasm_bindgen]
pub fn harnesses() -> String {
    let list: Vec<Value> = Harness::ALL
        .iter()
        .map(|h| {
            json!({
                "harness": h,
                "id": h.id(),
                "program": h.program(),
                "display_name": h.display_name(),
                "native_acp": h.native_acp(),
            })
        })
        .collect();
    to_json(&list)
}

/// Bytes a terminal writes back to answer an OSC 7501 probe.
#[wasm_bindgen]
pub fn osc7501_probe_reply() -> Vec<u8> {
    osc7501::PROBE_REPLY.to_vec()
}

/// Incremental OSC 7501 extractor for a terminal byte stream (e.g. xterm.js output).
#[wasm_bindgen]
#[derive(Debug, Default)]
pub struct Osc7501Scanner {
    inner: osc7501::Scanner,
}

#[wasm_bindgen]
impl Osc7501Scanner {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a JSON array of messages completed by these bytes.
    pub fn feed(&mut self, bytes: &[u8]) -> String {
        to_json(&self.inner.feed(bytes))
    }
}

/// A command to run on the host, for example over an SSH exec channel:
/// write `stdin` to it (when not empty), then close it.
#[wasm_bindgen(getter_with_clone)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccExec {
    /// A POSIX `sh` script.
    pub script: String,
    pub stdin: Vec<u8>,
}

impl From<Exec> for AccExec {
    fn from(e: Exec) -> Self {
        Self {
            script: e.script,
            stdin: e.stdin,
        }
    }
}

impl AccExec {
    fn script(script: String) -> Self {
        Self {
            script,
            stdin: Vec::new(),
        }
    }
}

/// Throws an `Error` named `AccError` whose `kind` says what went wrong.
pub(crate) fn error(kind: &str, message: &str) -> JsValue {
    let e = js_sys::Error::new(message);
    e.set_name("AccError");
    // Setting a property on a fresh object can't fail.
    let _ = js_sys::Reflect::set(&e, &JsValue::from_str("kind"), &JsValue::from_str(kind));
    e.into()
}

pub(crate) fn to_json<T: Serialize + ?Sized>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

pub(crate) fn from_json<T: DeserializeOwned>(json: &str, what: &str) -> Result<T, JsValue> {
    serde_json::from_str(json).map_err(|e| error("InvalidJson", &format!("unreadable {what}: {e}")))
}

/// A harness by the name records use (`claude`) or its id (`claude-code`).
pub(crate) fn harness(name: &str) -> Result<Harness, JsValue> {
    Harness::ALL
        .into_iter()
        .find(|h| to_json(h).trim_matches('"') == name || h.id() == name)
        .ok_or_else(|| error("InvalidJson", &format!("unknown harness {name:?}")))
}

/// An item as the core serializes it, plus its harness-independent `detail`.
pub(crate) fn item_json(i: &Item, h: Harness) -> Value {
    let mut v = serde_json::to_value(i).unwrap_or_default();
    if let Some(o) = v.as_object_mut() {
        o.insert(
            "detail".into(),
            serde_json::to_value(i.detail(h)).unwrap_or_default(),
        );
    }
    v
}

/// JS numbers carry milliseconds and byte offsets exactly up to 2^53.
pub(crate) fn ms(n: f64) -> u64 {
    n.max(0.0) as u64
}

pub(crate) fn num(n: u64) -> f64 {
    n as f64
}
