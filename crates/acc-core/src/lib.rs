//! Agent Control Core.
//!
//! `acc-core` is **sans-IO**: it never opens sockets, spawns processes or starts
//! an async runtime. Drivers turn bytes from a harness into [`Event`]s and turn
//! commands into bytes for the harness. Transports (local process, SSH exec
//! channel, detached FIFO, multiplexer pane) live outside the core and only move
//! bytes. This keeps the core identical on iOS, Android, wasm32 and desktop, and
//! makes every protocol testable from recorded fixtures.
//!
//! Layout:
//! - [`harness`]: the six supported CLI harnesses and how to launch them headless.
//! - [`state`]: canonical session state (OSC 7501 vocabulary) and ranked sources.
//! - [`event`]: the normalized event stream every driver produces.
//! - [`command`]: canonical commands and harness options.
//! - [`driver`]: the sans-IO driver trait.
//! - [`pi`]: the pi driver (`--mode rpc`) and pi transcript reader.
//! - [`claude`], [`codex`]: drivers for Claude Code stream-json and Codex app-server.
//! - [`ndjson`], [`osc7501`]: incremental parsers used by drivers and terminals.
//! - [`detached`]: shell scripts for daemonless, disconnect-proof sessions.
//! - [`multiplexer`]: locating harnesses inside tmux, zellij, screen and Rex.
//! - [`acp`]: the Agent Client Protocol driver (Cursor, Copilot, opencode).
//! - [`session`]: a driver picked by harness, with timestamps and merged state;
//!   what the Swift, Kotlin and JS bindings wrap.

pub mod claude;
pub mod codex;
pub mod command;
pub mod detached;
pub mod driver;
pub mod event;
pub mod harness;
// Harness drivers.
pub mod acp;
pub mod multiplexer;
pub mod ndjson;
pub mod osc7501;
pub mod pi;
pub mod session;
pub mod shell;
pub mod state;
mod wire;

pub use event::Event;
pub use harness::Harness;
pub use state::{BlockKind, RunState, SessionState, StateSource};

/// Version of this crate, for clients to show and compare across host and client.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
