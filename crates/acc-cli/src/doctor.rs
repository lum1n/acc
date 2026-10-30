//! `acc doctor`: what this machine has that ACC can drive.

use std::process::{Command, ExitCode};

use acc_core::Harness;
use acc_core::harness::Compat;
use acc_core::sandbox::{self, Applied};

/// First output line of `program arg`, if the program runs at all.
fn version(program: &str, arg: &str) -> Option<String> {
    let o = Command::new(program).arg(arg).output().ok()?;
    let text = if o.stdout.is_empty() { o.stderr } else { o.stdout };
    let line = String::from_utf8_lossy(&text).lines().next()?.trim().to_owned();
    (o.status.success() || !line.is_empty()).then_some(line)
}

/// The short name `acc` accepts for a harness (`claude`, `cursor`, ...).
pub fn name(h: Harness) -> String {
    format!("{h:?}").to_lowercase()
}

/// How `version` compares with the versions ACC was tested with, as a
/// note for the end of a line (empty when it is in the tested range).
pub fn compat_note(h: Harness, version: &str) -> String {
    let (min, max) = h.tested_versions();
    match h.compat(version) {
        Compat::Tested => String::new(),
        Compat::Newer => format!(" (newer than tested: {min} to {max})"),
        Compat::Older => format!(" (older than tested: {min} to {max}; may not start)"),
        Compat::Unknown => format!(" (unknown version; tested {min} to {max})"),
    }
}

pub fn doctor() -> ExitCode {
    println!("harnesses");
    for h in Harness::ALL {
        match version(h.program(), "--version") {
            Some(v) => println!(
                "  ✓ {:<9} {:<9} {v:<32} try: acc run {}{}",
                name(h),
                h.program(),
                name(h),
                compat_note(h, &v)
            ),
            None => println!("  · {:<9} {:<9} not found", name(h), h.program()),
        }
    }
    println!("sandbox");
    let probe = Command::new("sh").arg("-c").arg(sandbox::probe_script()).output();
    let support = probe
        .ok()
        .and_then(|o| sandbox::parse_probe(&String::from_utf8_lossy(&o.stdout)));
    match support.map(|s| s.applied) {
        Some(Applied::Seatbelt) => println!("  ✓ Seatbelt (sandbox-exec)  try: acc run claude --sandbox"),
        Some(Applied::Bwrap) => println!("  ✓ bubblewrap               try: acc run claude --sandbox"),
        Some(Applied::None { reason }) => {
            println!("  · unavailable: {reason}; --sandbox runs harnesses unconfined");
        }
        None => println!("  · couldn't check"),
    }
    ExitCode::SUCCESS
}
