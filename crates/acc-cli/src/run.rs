//! `acc run`: drives Claude, Codex or pi as a local child process, the same
//! way `acc acp` drives an ACP harness. See [`crate::live`] for the shared
//! drive loop.

use std::process::ExitCode;

use acc_core::Harness;
use acc_core::claude::{ClaudeConfig, ClaudeDriver};
use acc_core::codex::{CodexConfig, CodexDriver};
use acc_core::driver::Driver;
use acc_core::pi::PiDriver;

use crate::live;

pub fn run(harness: Harness, args: &[&str]) -> Result<ExitCode, String> {
    let (cwd, flags) = args.split_first().ok_or("missing <cwd>")?;
    let mut resume = None;
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
            "--resume" => resume = Some(value()?),
            "--model" => model = Some(value()?),
            "--program" => program = Some(value()?),
            other => return Err(format!("unknown flag {other:?}")),
        }
    }
    let cwd = std::fs::canonicalize(cwd).map_err(|e| format!("{cwd}: {e}"))?;
    let driver: Box<dyn Driver> = match harness {
        Harness::Claude => Box::new(ClaudeDriver::new(ClaudeConfig {
            model,
            ..ClaudeConfig::default()
        })),
        // Codex takes its working directory and model in `thread/start`,
        // not as launch flags; resume is in-protocol too (`thread/resume`),
        // so it goes in the config rather than to `driver.launch`.
        Harness::Codex => Box::new(CodexDriver::new(CodexConfig {
            resume: resume.clone(),
            cwd: Some(cwd.to_string_lossy().into_owned()),
            model,
            ..CodexConfig::default()
        })),
        Harness::Pi => {
            if model.is_some() {
                return Err("pi has no model at spawn; set it live with a set_model command".into());
            }
            Box::new(PiDriver::new())
        }
        other => return Err(format!("{} runs over acc acp, not acc run", other.display_name())),
    };
    live::drive(driver, &cwd, resume.as_deref(), program.as_deref())
}
