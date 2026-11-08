//! Agent Control Core.
//!
//! `acc-core` is **sans-IO**: it never opens sockets, spawns processes or starts
//! an async runtime. Drivers turn bytes from a harness into [`Event`]s and turn
//! commands into bytes for the harness. Transports (local process, SSH exec
//! channel, detached FIFO) live outside the core and only move bytes. This
//! keeps the core identical on iOS, Android, wasm32 and desktop, and makes
//! every protocol testable from recorded fixtures.
//!
//! Features:
//! - `drivers` (default): the protocol drivers, sessions, policy, handoff,
//!   usage and loadout. Without it the crate is the host side alone:
//!   [`harness`], [`detached`], [`sandbox`], [`worktree`], [`watch`],
//!   [`shell`], [`exec`] and [`state`], for an embedder that provisions
//!   workspaces and inspects sessions but drives none.
//! - `host`: code that spawns local processes. Off for wasm32 and mobile.
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
//! - [`sandbox`]: confining a harness with Seatbelt (macOS) or bubblewrap (Linux).
//! - [`watch`]: session events recorded on the host while no client is connected.

// Host side: what an embedder needs to provision and inspect sessions
// without driving one. Always built.
pub mod detached;
pub mod exec;
pub mod harness;
pub mod sandbox;
pub mod shell;
pub mod state;
pub mod watch;
// Only `one_line` is used without the drivers.
#[cfg_attr(not(feature = "drivers"), allow(dead_code))]
mod wire;
pub mod worktree;

// Driver side (feature `drivers`, on by default): the protocol drivers and
// everything that reads what a harness says.
#[cfg(feature = "drivers")]
pub mod acp;
#[cfg(feature = "drivers")]
pub mod claude;
#[cfg(feature = "drivers")]
pub mod codex;
#[cfg(feature = "drivers")]
pub mod command;
#[cfg(feature = "drivers")]
pub mod driver;
#[cfg(feature = "drivers")]
pub mod event;
#[cfg(feature = "drivers")]
pub mod handoff;
#[cfg(feature = "drivers")]
pub mod item;
#[cfg(feature = "drivers")]
pub mod live;
#[cfg(feature = "drivers")]
pub mod loadout;
#[cfg(feature = "drivers")]
pub mod ndjson;
#[cfg(feature = "drivers")]
pub mod pi;
#[cfg(feature = "drivers")]
pub mod policy;
#[cfg(feature = "drivers")]
pub mod question;
#[cfg(feature = "drivers")]
pub mod session;
#[cfg(feature = "drivers")]
pub mod transcript;
#[cfg(feature = "drivers")]
pub mod usage;

#[cfg(feature = "drivers")]
pub use event::Event;
pub use exec::Exec;
pub use harness::Harness;
pub use state::{BlockKind, RunState, SessionState, StateSource};

/// Version of this crate, for clients to show and compare across host and client.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
