//! wasm-bindgen surface of acc-core for browsers and Node.
//!
//! Values cross the boundary as JSON strings shaped exactly like the core's
//! serde types, so TypeScript types can be generated from the same structs.

use acc_core::{Harness, osc7501};
use wasm_bindgen::prelude::*;

/// Version of the core library.
#[wasm_bindgen]
pub fn version() -> String {
    acc_core::VERSION.to_owned()
}

/// JSON array of supported harness ids.
#[wasm_bindgen]
pub fn harnesses() -> String {
    serde_json::to_string(&Harness::ALL).unwrap_or_else(|_| "[]".to_owned())
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
        serde_json::to_string(&self.inner.feed(bytes)).unwrap_or_else(|_| "[]".to_owned())
    }
}
