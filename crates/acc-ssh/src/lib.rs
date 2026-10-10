//! Daemonless SSH transport for acc-core.
//!
//! Drives harnesses on a host over a plain SSH connection with nothing
//! installed there: every remote action is a short POSIX shell script from
//! [`acc_core::detached`] run over an SSH exec channel. The SSH client is
//! [russh](https://docs.rs/russh) (pure Rust, ring crypto), so the same code
//! embeds in iOS and Android apps without an OpenSSH binary.
//!
//! Layers, bottom up:
//! - [`Host`]: one SSH connection that redials on demand, runs scripts and
//!   opens streaming channels.
//! - [`Detached`]: the bytes of one detached session (harness stdin on a FIFO,
//!   stdout in a log). Reads resume from the exact byte offset after a
//!   dropped connection; writes are delivered whole and at most once.
//! - [`Session`]: an [`acc_core::session::Session`] (driver picked by
//!   harness, merged state) on top of a [`Detached`], turning commands into
//!   writes and log bytes into events.
//!
//! The harness never notices the client leaving: it keeps running detached
//! on the host, approvals wait for an answer, and any client that reconnects
//! (the same phone later, or the desktop) picks up from its offset. See
//! `research/daemonless-ssh-control.md`.
//!
//! Everything is async on tokio; the caller owns the runtime.

mod agent;
mod detached;
mod error;
mod host;
mod session;

pub use detached::{Detached, Read, Retry, list};
pub use error::Error;
pub use host::{AgentSocket, Auth, Host, HostKeyCheck, Output, SshConfig, Stream, StreamMsg};
pub use session::Session;

/// Re-exported so callers can name session listings and statuses.
pub use acc_core::detached::{Listed, SessionSpec, Status};
/// Re-exported so callers can configure a [`Session`] without naming acc-core.
pub use acc_core::session::{Opening, SessionConfig};

pub type Result<T, E = Error> = std::result::Result<T, E>;
