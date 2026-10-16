//! Agent Control Core.
//!
//! `acc-core` is **sans-IO**: it never opens sockets, spawns processes or starts
//! an async runtime. Drivers turn bytes from a harness into [`Event`]s and turn
//! commands into bytes for the harness. Transports (local process, SSH exec
//! channel, detached FIFO) live outside the core and only move bytes. This
//! keeps the core identical on iOS, Android, wasm32 and desktop, and makes
//! every protocol testable from recorded fixtures.
//!
//! Layout:
//! - [`harness`]: the six supported CLI harnesses and how to launch them headless.
//! - [`state`]: canonical session state (OSC 7501 vocabulary) and ranked sources.
//! - [`event`]: the normalized event stream every driver produces.
//! - [`command`]: canonical commands and harness options.
//! - [`driver`]: the sans-IO driver trait.
//! - [`pi`]: the pi driver (`--mode rpc`).
//! - [`claude`], [`codex`]: drivers for Claude Code stream-json and Codex app-server.
//! - [`ndjson`]: the incremental line parser drivers use.
//! - [`exec`]: a command for the embedder to run on the host.
//! - [`detached`]: shell scripts for daemonless, disconnect-proof sessions.
//! - [`session`]: a driver bound to a detached session: launch, attach with
//!   replay, and the folded transcript, state, usage and permissions.
//! - [`item`], [`transcript`]: typed item details (commands, tools, diffs) and
//!   the per-session item list.
//! - [`acp`]: the Agent Client Protocol driver (Cursor, Copilot, opencode).
//! - [`live`]: a driver picked by harness, with timestamps and merged state,
//!   for embedders that own a live byte stream (what `acc-ssh` builds on).
//! - [`question`]: multi-part questions and how to answer all of them.
//! - [`policy`]: allow and deny rules answered for every harness.
//! - [`handoff`]: priming another harness with a conversation so far.
//! - [`usage`]: what sessions spent, added up per account and per day.
//! - [`worktree`]: a git worktree per session, its diff, merge and discard.
//! - [`watch`]: session events recorded on the host while no client is connected.

pub mod claude;
pub mod codex;
pub mod command;
pub mod detached;
pub mod driver;
pub mod event;
pub mod exec;
pub mod handoff;
pub mod harness;
pub mod item;
pub mod live;
pub mod loadout;
// Harness drivers.
pub mod acp;
pub mod ndjson;
pub mod pi;
pub mod policy;
pub mod question;
pub mod session;
pub mod shell;
pub mod state;
pub mod transcript;
pub mod usage;
pub mod watch;
mod wire;
pub mod worktree;

pub use event::Event;
pub use exec::Exec;
pub use harness::Harness;
pub use state::{BlockKind, RunState, SessionState, StateSource};

/// Version of this crate, for clients to show and compare across host and client.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
