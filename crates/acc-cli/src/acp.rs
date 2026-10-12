//! `acc acp`: drives an ACP harness as a local child process. See
//! [`crate::live`] for the shared drive loop.

use std::process::ExitCode;

use acc_core::Harness;
use acc_core::acp::{AcpConfig, AcpDriver, Start};

use crate::live;

pub fn run(harness: Harness, args: &[&str]) -> Result<ExitCode, String> {
    let (cwd, flags) = args.split_first().ok_or("missing <cwd>")?;
    let mut start = Start::New;
    let mut model = None;
    let mut program = None;
    let mut it = flags.iter();
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .map(|v| (*v).to_owned())
                .ok_or(format!("{flag} needs a value"))
        };
        match *flag {
            "--resume" => start = Start::Resume { session_id: value()? },
            "--model" => model = Some(value()?),
            "--program" => program = Some(value()?),
            other => return Err(format!("unknown flag {other:?}")),
        }
    }
    let cwd = std::fs::canonicalize(cwd).map_err(|e| format!("{cwd}: {e}"))?;
    let mut cfg = AcpConfig::new(cwd.to_string_lossy(), start);
    cfg.model = model;
    cfg.client_tag = format!("cli{}", std::process::id());
    let driver = AcpDriver::new(harness, cfg);
    live::drive(Box::new(driver), &cwd, None, program.as_deref())
}
