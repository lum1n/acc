//! Confining a session's harness: Seatbelt on macOS, bubblewrap on Linux.
//!
//! A [`Sandbox`] says what a session may do; [`rules`] resolves it for one
//! harness into [`Rules`] (kept in the session's [`LaunchSpec`]), and
//! [`argv_script`] renders those into a `sh` fragment for the launch
//! script. The fragment picks the mechanism from `uname` on the host at
//! launch time, so the same rules work on a Mac and on a Linux box reached
//! over SSH, and nothing is installed. A host that can't sandbox (no
//! `bwrap`, user namespaces blocked, `sandbox-exec` missing or refusing)
//! still starts the harness, unconfined, and says so ([`Applied::None`]).
//!
//! Under the default preset the harness reads the whole filesystem except
//! credentials, and writes only its working directory, the repository's git
//! directory, temporary directories, package caches and its own state.
//! The network stays on.

use serde::{Deserialize, Serialize};

use crate::detached::quote_path;
use crate::harness::{Harness, LaunchSpec};
use crate::shell::{join, quote};

/// How much of the workspace a sandboxed session may change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    /// The working directory and its repository are writable.
    #[default]
    Workspace,
    /// Nothing in the workspace is writable; the harness's state,
    /// temporary directories and caches still are, or harnesses break.
    ReadOnly,
}

/// What a sandboxed session may do. `Sandbox::default()` is the
/// opinionated preset: workspace writable, credentials hidden.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Sandbox {
    pub preset: Preset,
    /// Extra writable paths, absolute or `~/...`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub write: Vec<String>,
    /// Extra paths made unreadable, absolute or `~/...`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hide: Vec<String>,
    /// Leave the usual credential files ([`CREDENTIALS`]) readable.
    pub show_credentials: bool,
}

/// The paths a [`Sandbox`] resolves to for one harness. Paths are absolute
/// or `~/...`; the launch script resolves them on the host.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rules {
    /// The working directory and its repository's git directory
    /// (`git rev-parse --git-common-dir`, so commits from worktrees work)
    /// are writable.
    pub workspace: bool,
    /// Writable paths besides the workspace, temporary directories and
    /// terminals, which always are.
    pub write: Vec<String>,
    /// Unreadable paths.
    pub hide: Vec<String>,
    /// Paths inside hidden ones that stay readable (`~/.ssh/config`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub show: Vec<String>,
    /// Harness flags added under Seatbelt, which can't nest: they turn the
    /// harness's own sandbox off, since ACC's already confines it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub seatbelt_args: Vec<String>,
}

/// Package caches every harness's tools may fill.
pub const CACHES: &[&str] = &[
    "~/.cache",
    "~/.npm",
    "~/.cargo/registry",
    "~/.cargo/git",
    "~/go/pkg/mod",
    "~/.bun/install/cache",
    "~/Library/Caches",
];

/// Credentials the default preset hides. `ssh-agent` and `gh`
/// (`~/.config/gh`) keep working, so agents can still push and open PRs.
pub const CREDENTIALS: &[&str] = &[
    "~/.ssh",
    "~/.aws",
    "~/.gnupg",
    "~/.kube",
    "~/.config/gcloud",
    "~/.azure",
    "~/.docker/config.json",
    "~/.netrc",
    "~/.git-credentials",
    "~/.password-store",
];

/// Files inside [`CREDENTIALS`] that stay readable, so `ssh` and `git`
/// over SSH still find their hosts.
pub const SHOWN_CREDENTIALS: &[&str] = &["~/.ssh/config", "~/.ssh/known_hosts"];

/// Where a harness keeps its login, settings and sessions.
pub fn state_paths(harness: Harness) -> &'static [&'static str] {
    match harness {
        Harness::Claude => &[
            "~/.claude",
            "~/.claude.json",
            "~/.claude.json.lock",
            "~/.claude.json.backup",
            "~/.local/state/claude",
            "~/.local/share/claude",
        ],
        Harness::Codex => &["~/.codex"],
        Harness::Cursor => &["~/.cursor", "~/.config/cursor", "~/.local/share/cursor-agent"],
        Harness::Copilot => &["~/.copilot", "~/.config/github-copilot"],
        Harness::Pi => &["~/.pi"],
        Harness::Opencode => &[
            "~/.config/opencode",
            "~/.local/share/opencode",
            "~/.local/state/opencode",
            "~/.cache/opencode",
        ],
    }
}

/// Flags that turn a harness's own sandbox off (see [`Rules::seatbelt_args`]).
fn nested_off(harness: Harness) -> &'static [&'static str] {
    match harness {
        Harness::Codex => &["-c", "sandbox_mode=\"danger-full-access\""],
        Harness::Claude => &["--settings", r#"{"sandbox":{"enabled":false}}"#],
        _ => &[],
    }
}

/// Resolves `sandbox` for `harness`.
pub fn rules(harness: Harness, sandbox: &Sandbox) -> Rules {
    let owned = |v: &[&str]| v.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>();
    let mut write = owned(state_paths(harness));
    write.extend(owned(CACHES));
    write.extend(sandbox.write.iter().cloned());
    let (mut hide, show) = if sandbox.show_credentials {
        (Vec::new(), Vec::new())
    } else {
        (owned(CREDENTIALS), owned(SHOWN_CREDENTIALS))
    };
    hide.extend(sandbox.hide.iter().cloned());
    Rules {
        workspace: sandbox.preset == Preset::Workspace,
        write,
        hide,
        show,
        seatbelt_args: owned(nested_off(harness)),
    }
}

/// What the host did when the session started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Applied {
    /// macOS `sandbox-exec`.
    Seatbelt,
    /// Linux bubblewrap.
    Bwrap,
    /// The harness runs unconfined, for `reason`.
    None { reason: String },
}

impl Applied {
    /// Parses one line the launch script recorded (`seatbelt`, `bwrap` or
    /// `none <reason>`).
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        match line.split_once(' ').map_or((line, ""), |(a, b)| (a, b)) {
            ("seatbelt", _) => Some(Applied::Seatbelt),
            ("bwrap", _) => Some(Applied::Bwrap),
            ("none", reason) => Some(Applied::None {
                reason: if reason.is_empty() {
                    "unknown".into()
                } else {
                    reason.to_owned()
                },
            }),
            _ => None,
        }
    }

    /// The text of an [`crate::Event::Notice`] when the session isn't confined.
    pub fn notice(&self) -> Option<String> {
        match self {
            Applied::None { reason } => Some(format!("Not sandboxed, running unconfined: {reason}")),
            _ => None,
        }
    }
}

/// Resolves a path to its real location, symlinks included: Seatbelt
/// matches real paths (macOS's `/tmp` is `/private/tmp`) and `bwrap` can't
/// mount over a symlink (a dotfiles-managed `~/.config/opencode`). A missing
/// path keeps its name under its parent's real path. `readlink -f` is
/// missing on older macOS, hence the fallback.
const REALPATH: &str = r#"acc_rp() { if acc_r=$(readlink -f -- "$1" 2>/dev/null) && [ -n "$acc_r" ]; then printf '%s\n' "$acc_r"; elif [ -d "$1" ]; then (cd "$1" && pwd -P); elif acc_p=$(cd "$(dirname "$1")" 2>/dev/null && pwd -P); then printf '%s/%s\n' "${acc_p%/}" "$(basename "$1")"; else printf '%s\n' "$1"; fi; }
"#;

/// The Seatbelt profile for `rules`, with every path a `-D` parameter
/// ([`argv_script`] passes them), so nothing user-given is spliced in.
/// Later rules win: everything is allowed, then writes are denied, then
/// the writable paths allowed, then hidden paths denied, then shown ones
/// allowed again.
pub fn seatbelt_profile(rules: &Rules) -> String {
    let params = |prefix: &str, n: usize| -> String {
        (0..n)
            .map(|i| format!(" (subpath (param \"{prefix}{i}\"))"))
            .collect()
    };
    let mut p = String::from(
        "(version 1)\n(allow default)\n(deny file-write*)\n(allow file-write*\
         \n (subpath (param \"TMP\")) (subpath \"/private/tmp\") (subpath \"/private/var/folders\")\
         \n (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/tty\") (literal \"/dev/ptmx\")\
         \n (literal \"/dev/dtracehelper\") (regex #\"^/dev/ttys[0-9]+$\") (regex #\"^/dev/fd/\")",
    );
    if rules.workspace {
        p.push_str("\n (subpath (param \"CWD\")) (subpath (param \"GIT\"))");
    }
    if !rules.write.is_empty() {
        p.push('\n');
        p.push_str(&params("W", rules.write.len()));
    }
    p.push_str(")\n");
    if !rules.hide.is_empty() {
        p.push_str(&format!(
            "(deny file-read* file-write*{})\n",
            params("H", rules.hide.len())
        ));
    }
    if !rules.show.is_empty() {
        p.push_str(&format!("(allow file-read*{})\n", params("S", rules.show.len())));
    }
    p
}

/// A `sh` fragment that leaves the harness's full command line (sandbox
/// wrapper, `env`, program, arguments) in `"$@"` and what it applied in
/// `$acc_sb` (`seatbelt`, `bwrap` or `none <reason>`). Run it in the
/// session's working directory, then `"$@"` (or `exec "$@"`).
///
/// On Linux the harness runs under `bwrap --ro-bind / /` with the writable
/// paths bound read-write, hidden directories under an empty tmpfs and
/// hidden files under `/dev/null`. There is no `--unshare-pid`, so the PID
/// the launch script records is `bwrap`'s and stopping it takes the
/// harness down (`--die-with-parent`). Only paths that exist at launch
/// can be bound, so a state directory created later stays read-only.
/// On macOS it runs under
/// `sandbox-exec` with [`seatbelt_profile`], plus [`Rules::seatbelt_args`].
/// Each mechanism is tried once with `true` first; if that fails the
/// harness runs unconfined.
pub fn argv_script(rules: &Rules, spec: &LaunchSpec) -> String {
    let mut s = String::from(REALPATH);
    s.push_str(
        "acc_cwd=$(pwd -P)\n\
         acc_git=$(git rev-parse --git-common-dir 2>/dev/null) && acc_git=$(acc_rp \"$acc_git\") || acc_git=\n\
         set --\n\
         case $(uname -s) in\n\
         Darwin)\n\
         if ! command -v sandbox-exec >/dev/null 2>&1; then acc_sb='none sandbox-exec not found'; else\n\
         set -- sandbox-exec -p ",
    );
    s.push_str(&quote(&seatbelt_profile(rules)));
    s.push_str(" -D \"TMP=$(acc_rp \"${TMPDIR:-/tmp}\")\"");
    if rules.workspace {
        s.push_str(" -D \"CWD=$acc_cwd\" -D \"GIT=${acc_git:-$acc_cwd}\"");
    }
    s.push('\n');
    for (prefix, paths) in [("W", &rules.write), ("H", &rules.hide), ("S", &rules.show)] {
        for (i, path) in paths.iter().enumerate() {
            s.push_str(&format!(
                "set -- \"$@\" -D \"{prefix}{i}=$(acc_rp {})\"\n",
                quote_path(path)
            ));
        }
    }
    s.push_str(
        "if acc_e=$(\"$@\" /usr/bin/true 2>&1); then acc_sb=seatbelt; \
         else acc_sb=\"none sandbox-exec failed: $(printf '%s\\n' \"$acc_e\" | head -n 1)\"; set --; fi\n\
         fi ;;\n\
         Linux)\n\
         if ! command -v bwrap >/dev/null 2>&1; then acc_sb='none bwrap not found'; else\n\
         set -- bwrap --ro-bind / / --dev /dev --die-with-parent --new-session \
         --bind-try /tmp /tmp --bind-try /var/tmp /var/tmp --bind-try /dev/shm /dev/shm\n\
         [ -n \"$TMPDIR\" ] && set -- \"$@\" --bind-try \"$TMPDIR\" \"$TMPDIR\"\n",
    );
    if rules.workspace {
        s.push_str(
            "set -- \"$@\" --bind \"$acc_cwd\" \"$acc_cwd\"\n\
             [ -n \"$acc_git\" ] && set -- \"$@\" --bind \"$acc_git\" \"$acc_git\"\n",
        );
    }
    for path in &rules.write {
        s.push_str(&format!(
            "acc_x=$(acc_rp {}); set -- \"$@\" --bind-try \"$acc_x\" \"$acc_x\"\n",
            quote_path(path)
        ));
    }
    for path in &rules.hide {
        s.push_str(&format!(
            "acc_x=$(acc_rp {}); if [ -d \"$acc_x\" ]; then set -- \"$@\" --tmpfs \"$acc_x\"; \
             elif [ -e \"$acc_x\" ]; then set -- \"$@\" --ro-bind /dev/null \"$acc_x\"; fi\n",
            quote_path(path)
        ));
    }
    for path in &rules.show {
        s.push_str(&format!(
            "acc_x=$(acc_rp {}); set -- \"$@\" --ro-bind-try \"$acc_x\" \"$acc_x\"\n",
            quote_path(path)
        ));
    }
    s.push_str(
        "set -- \"$@\" --\n\
         if acc_e=$(\"$@\" true 2>&1); then acc_sb=bwrap; \
         else acc_sb=\"none bwrap failed: $(printf '%s\\n' \"$acc_e\" | head -n 1)\"; set --; fi\n\
         fi ;;\n\
         *) acc_sb=\"none no sandbox on $(uname -s)\" ;;\n\
         esac\n",
    );
    let mut cmd: Vec<String> = Vec::new();
    if !spec.env.is_empty() {
        cmd.push("env".into());
        cmd.extend(spec.env.iter().map(|(k, v)| format!("{k}={v}")));
    }
    cmd.push(spec.program.clone());
    cmd.extend(spec.args.iter().cloned());
    s.push_str(&format!(
        "set -- \"$@\" {}\n",
        join(cmd.iter().map(String::as_str))
    ));
    if !rules.seatbelt_args.is_empty() {
        s.push_str(&format!(
            "if [ \"$acc_sb\" = seatbelt ]; then set -- \"$@\" {}; fi\n",
            join(rules.seatbelt_args.iter().map(String::as_str))
        ));
    }
    s
}

/// What a host can do, from [`probe_script`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Support {
    /// `uname -s`.
    pub os: String,
    /// What a sandboxed session would get there now.
    pub applied: Applied,
}

/// Script that checks whether the host can sandbox a session. Parse its
/// output with [`parse_probe`].
pub fn probe_script() -> String {
    let spec = LaunchSpec {
        program: "true".into(),
        args: Vec::new(),
        env: Vec::new(),
        protocol: crate::harness::Protocol::Acp,
        sandbox: None,
    };
    let mut s = String::from(crate::detached::login_path_prelude());
    s.push_str("cd \"$HOME\" 2>/dev/null || cd /\n");
    s.push_str(&argv_script(&rules(Harness::Claude, &Sandbox::default()), &spec));
    s.push_str(
        "printf 'O\\t%s\\nB\\t%s\\n' \"$(uname -s)\" \"$acc_sb\"\n\
         case $acc_sb in *bwrap*) [ -r /proc/sys/kernel/apparmor_restrict_unprivileged_userns ] && \
         [ \"$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns)\" = 1 ] && \
         printf 'A\\t1\\n' ;; esac; true\n",
    );
    s
}

/// Parses the output of [`probe_script`]. On Ubuntu, where AppArmor keeps
/// unprivileged user namespaces from `bwrap`, the reason says so.
pub fn parse_probe(out: &str) -> Option<Support> {
    let field = |tag: &str| {
        out.lines()
            .find_map(|l| l.strip_prefix(tag).and_then(|l| l.strip_prefix('\t')))
    };
    let os = field("O")?.trim().to_owned();
    let mut applied = Applied::parse(field("B")?)?;
    if let (Some(_), Applied::None { reason }) = (field("A"), &mut applied) {
        reason.push_str(
            " (AppArmor restricts unprivileged user namespaces: \
             sysctl kernel.apparmor_restrict_unprivileged_userns=0, or an AppArmor profile for bwrap)",
        );
    }
    Some(Support { os, applied })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_preset_writes_workspace_and_state_and_hides_credentials() {
        let r = rules(Harness::Claude, &Sandbox::default());
        assert!(r.workspace);
        assert!(r.write.iter().any(|p| p == "~/.claude.json"));
        assert!(r.write.iter().any(|p| p == "~/.cache"));
        assert!(r.hide.iter().any(|p| p == "~/.ssh"));
        assert_eq!(r.show, SHOWN_CREDENTIALS);
        assert!(r.seatbelt_args.contains(&"--settings".to_owned()));
        for h in Harness::ALL {
            let r = rules(h, &Sandbox::default());
            for p in state_paths(h) {
                assert!(r.write.iter().any(|w| w == p), "{h:?} {p}");
            }
            assert!(!r.write.iter().any(|w| w == "~/.ssh"));
        }
        let codex = rules(Harness::Codex, &Sandbox::default());
        assert_eq!(codex.seatbelt_args, ["-c", "sandbox_mode=\"danger-full-access\""]);
        assert!(rules(Harness::Pi, &Sandbox::default()).seatbelt_args.is_empty());
    }

    #[test]
    fn read_only_extra_paths_and_shown_credentials() {
        let s = Sandbox {
            preset: Preset::ReadOnly,
            write: vec!["/data".into()],
            hide: vec!["~/secret".into()],
            show_credentials: true,
        };
        let r = rules(Harness::Pi, &s);
        assert!(!r.workspace);
        assert!(r.write.contains(&"/data".to_owned()));
        assert!(r.write.contains(&"~/.pi".to_owned()));
        assert_eq!(r.hide, ["~/secret"]);
        assert!(r.show.is_empty());
    }

    #[test]
    fn rules_round_trip_and_old_specs_parse() {
        let r = rules(Harness::Codex, &Sandbox::default());
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<Rules>(&json).unwrap(), r);
        let spec: LaunchSpec =
            serde_json::from_str(r#"{"program":"pi","args":[],"env":[],"protocol":"pi_rpc"}"#).unwrap();
        assert_eq!(spec.sandbox, None);
        let s: Sandbox = serde_json::from_str("{}").unwrap();
        assert_eq!(s, Sandbox::default());
        let s: Sandbox = serde_json::from_str(r#"{"preset":"read_only"}"#).unwrap();
        assert_eq!(s.preset, Preset::ReadOnly);
    }

    #[test]
    fn seatbelt_profile_uses_parameters_in_order() {
        let r = Rules {
            workspace: true,
            write: vec!["~/a\"b".into(), "/x".into()],
            hide: vec!["~/.ssh".into()],
            show: vec!["~/.ssh/config".into()],
            seatbelt_args: vec![],
        };
        let p = seatbelt_profile(&r);
        assert!(!p.contains("a\"b"), "paths are parameters, never spliced in");
        let deny = p.find("(deny file-write*)").unwrap();
        let allow = p.find("(param \"W1\")").unwrap();
        let hide = p
            .find("(deny file-read* file-write* (subpath (param \"H0\")))")
            .unwrap();
        let show = p.find("(allow file-read* (subpath (param \"S0\")))").unwrap();
        assert!(deny < allow && allow < hide && hide < show, "{p}");
        assert!(p.contains("(param \"CWD\")"));
        let ro = seatbelt_profile(&Rules {
            workspace: false,
            ..r
        });
        assert!(!ro.contains("CWD"));
    }

    #[test]
    fn argv_script_quotes_paths_and_adds_seatbelt_flags() {
        let mut r = rules(Harness::Codex, &Sandbox::default());
        r.write.push("/my dir/it's".into());
        let spec = Harness::Codex.launch(None);
        let s = argv_script(&r, &spec);
        assert!(
            s.contains("acc_x=$(acc_rp '/my dir/it'\\''s'); set -- \"$@\" --bind-try \"$acc_x\" \"$acc_x\"")
        );
        assert!(s.contains("acc_x=$(acc_rp \"$HOME\"/.codex); set -- \"$@\" --bind-try"));
        assert!(s.contains(
            "acc_x=$(acc_rp \"$HOME\"/.ssh); if [ -d \"$acc_x\" ]; then set -- \"$@\" --tmpfs \"$acc_x\""
        ));
        assert!(s.contains("acc_x=$(acc_rp \"$HOME\"/.ssh/config); set -- \"$@\" --ro-bind-try"));
        assert!(s.contains("set -- \"$@\" codex app-server\n"));
        assert!(s.contains(
            "if [ \"$acc_sb\" = seatbelt ]; then set -- \"$@\" -c 'sandbox_mode=\"danger-full-access\"'; fi"
        ));
    }

    #[test]
    fn applied_parses_and_notices() {
        assert_eq!(Applied::parse("bwrap\n"), Some(Applied::Bwrap));
        assert_eq!(Applied::parse("seatbelt"), Some(Applied::Seatbelt));
        let none = Applied::parse("none bwrap not found").unwrap();
        assert_eq!(
            none,
            Applied::None {
                reason: "bwrap not found".into()
            }
        );
        assert!(none.notice().unwrap().contains("bwrap not found"));
        assert_eq!(Applied::Bwrap.notice(), None);
        assert_eq!(Applied::parse("junk"), None);
    }

    #[test]
    fn probe_parses_and_explains_apparmor() {
        let s = parse_probe("noise\nO\tLinux\nB\tnone bwrap failed: setting up uid map\nA\t1\n").unwrap();
        assert_eq!(s.os, "Linux");
        let Applied::None { reason } = s.applied else {
            panic!()
        };
        assert!(reason.contains("AppArmor"));
        assert_eq!(
            parse_probe("O\tDarwin\nB\tseatbelt\n").map(|s| s.applied),
            Some(Applied::Seatbelt)
        );
        assert_eq!(parse_probe(""), None);
    }
}

/// The fragment under a real `sh`, against a temporary `HOME`. Unix only.
#[cfg(all(test, unix))]
#[allow(clippy::expect_used)]
pub(crate) mod sh_tests {
    use super::*;
    use std::process::Command;

    /// Whether this host can run the sandbox (bwrap or Seatbelt works).
    pub(crate) fn host_can_sandbox() -> bool {
        let out = Command::new("sh")
            .arg("-c")
            .arg(probe_script())
            .output()
            .expect("sh");
        parse_probe(&String::from_utf8_lossy(&out.stdout))
            .is_some_and(|s| !matches!(s.applied, Applied::None { .. }))
    }

    #[test]
    fn probe_script_runs_under_sh() {
        let out = Command::new("sh")
            .arg("-c")
            .arg(probe_script())
            .output()
            .expect("sh");
        let support = parse_probe(&String::from_utf8_lossy(&out.stdout)).expect("parses");
        assert!(!support.os.is_empty());
    }

    #[test]
    fn harness_writes_only_where_allowed() {
        if !host_can_sandbox() {
            eprintln!("skipped: this host can't sandbox");
            return;
        }
        // Not under the temporary directory, which the sandbox leaves writable.
        let home = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(format!("acc-sbx-{}", std::process::id()));
        let work = home.join("work");
        std::fs::create_dir_all(home.join(".ssh")).expect("mkdir");
        std::fs::create_dir_all(&work).expect("mkdir");
        // Only paths that exist at launch are bound; a symlinked one
        // (dotfiles) is bound where it points.
        std::fs::create_dir_all(home.join("dotfiles/pi")).expect("mkdir");
        std::os::unix::fs::symlink(home.join("dotfiles/pi"), home.join(".pi")).expect("symlink");
        std::fs::write(home.join(".ssh/id_test"), "KEY").expect("write");
        std::fs::write(home.join(".ssh/config"), "Host x").expect("write");
        let spec = LaunchSpec {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                "touch ok; touch ~/acc-escape; cat ~/.ssh/id_test; cat ~/.ssh/config; touch ~/.pi/state"
                    .into(),
            ],
            env: vec![],
            protocol: crate::harness::Protocol::PiRpc,
            sandbox: None,
        };
        let script = format!(
            "{}\"$@\"; printf '\\n%s\\n' \"$acc_sb\"",
            argv_script(&rules(Harness::Pi, &Sandbox::default()), &spec)
        );
        let out = Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(&work)
            .env("HOME", &home)
            .output()
            .expect("sh");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(work.join("ok").exists(), "cwd is writable: {stdout}");
        assert!(
            home.join("dotfiles/pi/state").exists(),
            "state is writable: {stdout}"
        );
        assert!(!home.join("acc-escape").exists(), "home is not writable");
        assert!(!stdout.contains("KEY"), "the key is hidden: {stdout}");
        assert!(stdout.contains("Host x"), "ssh config stays readable: {stdout}");
        let last = stdout.lines().last().unwrap_or_default();
        assert!(
            matches!(Applied::parse(last), Some(Applied::Bwrap | Applied::Seatbelt)),
            "{stdout}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}
