//! POSIX shell quoting for commands run on hosts over SSH.
//!
//! Every remote command ACC builds goes through [`quote`], so paths, prompts
//! and session ids can never break out of their argument.

/// Quotes one argument for a POSIX `sh`.
pub fn quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:@%+,".contains(&b))
    {
        return arg.to_owned();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('\'');
    for c in arg.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Joins a program and its arguments into one quoted command line.
pub fn join<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    parts.into_iter().map(quote).collect::<Vec<_>>().join(" ")
}

/// The command line to send over an SSH exec channel to run `script` with `/bin/sh`.
///
/// sshd hands the command to the user's login shell, which may be bash, zsh
/// or fish. The script travels as one single-quoted argument; backslashes are
/// moved outside the quotes because fish treats `\\` specially inside single
/// quotes and POSIX shells don't. csh-family login shells are not supported
/// (they reject newlines inside quotes).
pub fn ssh_command(script: &str) -> String {
    let mut out = String::with_capacity(script.len() + 24);
    out.push_str("exec /bin/sh -c '");
    for c in script.chars() {
        match c {
            '\'' => out.push_str(r"'\''"),
            '\\' => out.push_str(r"'\\'"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// Script prefix that adopts the `PATH` of the user's interactive login
/// shell. Non-interactive SSH commands usually miss nvm, `~/.local/bin` and
/// Homebrew, which is where most harnesses live. Start-up noise is filtered
/// out with a marker, and the current `PATH` is kept if this fails.
pub const LOGIN_PATH: &str = r#"acc_p=$("${SHELL:-/bin/sh}" -lic 'printf "\n__ACC_PATH=%s\n" "$PATH"' </dev/null 2>/dev/null | sed -n 's/^__ACC_PATH=//p' | tail -n 1)
[ -n "$acc_p" ] && PATH=$acc_p
export PATH
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_command_escapes_quotes_and_backslashes() {
        assert_eq!(ssh_command("echo hi"), "exec /bin/sh -c 'echo hi'");
        assert_eq!(ssh_command("echo 'a'"), r"exec /bin/sh -c 'echo '\''a'\'''");
        assert_eq!(ssh_command(r"a\n"), r"exec /bin/sh -c 'a'\\'n'");
    }

    #[test]
    fn leaves_safe_words_alone() {
        assert_eq!(quote("--resume"), "--resume");
        assert_eq!(quote("/home/v/.acc/s1"), "/home/v/.acc/s1");
    }

    #[test]
    fn quotes_spaces_quotes_and_empty() {
        assert_eq!(quote(""), "''");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), "'it'\\''s'");
        assert_eq!(quote("$(rm -rf ~)"), "'$(rm -rf ~)'");
    }
}

/// Runs the generated commands through real login shells. Unix only.
#[cfg(all(test, unix))]
#[allow(clippy::expect_used)]
mod sh_tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn ssh_command_round_trips_through_login_shells() {
        let script = "printf '%s|' \"it's\" 'a\\b' \"$((1+1))\"\necho done\n";
        for shell in ["sh", "bash", "zsh", "fish"] {
            // Not every machine has every shell; sh always exists.
            let Ok(out) = Command::new(shell).arg("-c").arg(ssh_command(script)).output() else {
                assert_ne!(shell, "sh");
                continue;
            };
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                "it's|a\\b|2|done\n",
                "via {shell}"
            );
        }
    }

    #[test]
    fn login_path_keeps_a_usable_path() {
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("{LOGIN_PATH}command -v sh"))
            .output()
            .expect("sh");
        assert!(out.status.success());
        assert!(!out.stdout.is_empty());
    }
}
