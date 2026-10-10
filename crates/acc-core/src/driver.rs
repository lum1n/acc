//! The sans-IO driver trait.
//!
//! A driver is a pure state machine: bytes from the harness go in, [`Event`]s
//! come out; [`Command`]s go in, bytes for the harness come out. The transport
//! that carries the bytes (local pipe, SSH channel, detached FIFO, multiplexer
//! pane) is not the driver's concern, so the same driver runs on every platform
//! and is tested by replaying recorded sessions.

use crate::command::{Command, CommandEntry, OptionDef};
use crate::event::Event;
use crate::harness::{Harness, LaunchSpec};

/// Errors a driver reports for a command it can't carry out.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DriverError {
    #[error("{harness:?} does not support {what} headlessly")]
    Unsupported { harness: Harness, what: &'static str },
    #[error("no session is active yet")]
    NotStarted,
    #[error("unknown permission request {0}")]
    UnknownRequest(String),
}

/// Translates between one harness protocol and ACC's model.
pub trait Driver: Send {
    fn harness(&self) -> Harness;

    /// How to start the harness for this driver, optionally resuming a session.
    fn launch(&self, resume: Option<&str>) -> LaunchSpec;

    /// Bytes to write right after the process starts (handshakes such as
    /// ACP/app-server `initialize`). Empty for harnesses that need none.
    fn handshake(&mut self) -> Vec<u8>;

    /// Feed bytes read from the harness; returns events in order.
    fn on_output(&mut self, bytes: &[u8]) -> Vec<Event>;

    /// Turn a command into bytes to write to the harness.
    fn command(&mut self, cmd: Command) -> Result<Vec<u8>, DriverError>;

    /// The current command palette (harness list merged with ACC's canonical commands).
    fn commands(&self) -> &[CommandEntry];

    /// The current options, as last reported by the harness.
    fn options(&self) -> &[OptionDef];
}
