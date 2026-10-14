//! `acc doctor` and `acc sessions`: what this machine has that ACC can drive.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::SystemTime;

use acc_core::Harness;
use acc_core::codex::rollout;
use acc_core::harness::Compat;
use acc_core::multiplexer;

const MULTIPLEXERS: [(&str, &str); 4] = [
    ("tmux", "-V"),
    ("zellij", "--version"),
    ("screen", "-v"),
    ("rex", "--version"),
];

/// First output line of `program arg`, if the program runs at all.
fn version(program: &str, arg: &str) -> Option<String> {
    let o = Command::new(program).arg(arg).output().ok()?;
    // `screen -v` exits non-zero on some versions, so take any output.
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

    println!("multiplexers");
    for (m, arg) in MULTIPLEXERS {
        match version(m, arg) {
            Some(v) => println!("  ✓ {m:<9} {v:<42} try: acc script mux {m} - - list | sh"),
            None => println!("  · {m:<9} not found"),
        }
    }
    let env: Vec<(String, String)> = std::env::vars().collect();
    match multiplexer::locate(env.iter().map(|(k, v)| (k.as_str(), v.as_str()))) {
        Some(p) => println!(
            "  this shell is in {:?} pane {} ({})",
            p.multiplexer, p.pane, p.server
        ),
        None => println!("  this shell is not inside a multiplexer"),
    }

    println!("session stores (resumable sessions)");
    for h in [Harness::Claude, Harness::Codex, Harness::Pi] {
        if let Some(root) = store(h) {
            let n = transcripts(h, &root).len();
            println!(
                "  {:<9} {:<36} {n:>4}   list: acc sessions {}",
                name(h),
                root.display(),
                name(h)
            );
        }
    }
    ExitCode::SUCCESS
}

pub fn sessions(harness: Harness, limit: usize) -> ExitCode {
    let Some(root) = store(harness) else {
        eprintln!(
            "acc: {} keeps its sessions behind its own protocol; resume one with `acc run {} --resume <id>`",
            harness.display_name(),
            name(harness)
        );
        return ExitCode::from(2);
    };
    let mut files = transcripts(harness, &root);
    files.sort_by_key(|f| std::cmp::Reverse(f.1));
    if files.is_empty() {
        println!("no sessions under {}", root.display());
    }
    let now = SystemTime::now();
    for (path, modified) in files.into_iter().take(limit) {
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        let id = match harness {
            Harness::Claude => path.file_stem().map(|s| s.to_string_lossy().into_owned()),
            Harness::Codex => rollout::thread_id_from_path(&rel).map(str::to_owned),
            // pi resumes by session file path.
            _ => Some(path.to_string_lossy().into_owned()),
        };
        let Some(id) = id else { continue };
        let age = now.duration_since(modified).map_or(0, |d| d.as_secs());
        // Claude and pi name the directory after the working directory.
        let place = match harness {
            Harness::Codex => String::new(),
            _ => rel.split('/').next().unwrap_or("").to_owned(),
        };
        println!(
            "{:>6}  acc run {} --resume {id}  {place}",
            ago(age),
            name(harness)
        );
    }
    ExitCode::SUCCESS
}

fn ago(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// Where a harness keeps session files on this machine.
fn store(h: Harness) -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    match h {
        Harness::Claude => Some(home.join(".claude/projects")),
        Harness::Codex => Some(
            std::env::var_os("CODEX_HOME")
                .map_or_else(|| home.join(".codex"), PathBuf::from)
                .join("sessions"),
        ),
        Harness::Pi => Some(home.join(".pi/agent/sessions")),
        _ => None,
    }
}

/// Every `.jsonl` at the depth `h` keeps session files under `root`, with its mtime.
fn transcripts(h: Harness, root: &Path) -> Vec<(PathBuf, SystemTime)> {
    fn walk(dir: &Path, depth: u8, out: &mut Vec<(PathBuf, SystemTime)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let path = e.path();
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() && depth > 0 {
                walk(&path, depth - 1, out);
            } else if path.extension().is_some_and(|x| x == "jsonl") {
                out.push((path, meta.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
            }
        }
    }
    let mut out = Vec::new();
    // Codex nests rollouts by date (YYYY/MM/DD); Claude keeps subagent
    // transcripts one level below the sessions, which must not count.
    let depth = if h == Harness::Codex { 3 } else { 1 };
    walk(root, depth, &mut out);
    out
}
