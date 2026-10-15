//! A command for the embedder to run on the host.

use serde::{Deserialize, Serialize};

/// A command to run on the host (for example over an SSH exec channel).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exec {
    /// A POSIX `sh` script. Every dynamic value in it is quoted.
    pub script: String,
    /// Bytes to write to the script's stdin, then close it. Long-running
    /// watch commands document whether stdin must instead stay open.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stdin: Vec<u8>,
}

impl Exec {
    pub fn new(script: String) -> Self {
        Self {
            script,
            stdin: Vec::new(),
        }
    }

    pub fn with_stdin(script: String, stdin: impl Into<Vec<u8>>) -> Self {
        Self {
            script,
            stdin: stdin.into(),
        }
    }
}
