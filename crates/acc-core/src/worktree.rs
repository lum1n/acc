//! A git worktree per session, so several agents can work on one repository
//! at once without touching each other's files or the user's checkout.
//!
//! [`add_script`] makes the worktree before launch and prints the directory
//! to launch in (absolute, as ACP harnesses need); the session's
//! [`crate::session::LaunchRequest::worktree`] names it. The session then
//! hands out [`diff_script`] (what the agent changed: a `git diff` for
//! [`crate::item::parse_unified`]), [`merge_script`] and [`discard_script`].
//!
//! Layout under `$HOME/.acc/w/`: `<name>/` is the worktree, on branch
//! `acc/<name>`, and `<name>.meta` holds three lines: the commit the
//! session's changes are counted from, the repository's top level and the
//! branch. Only `git` and POSIX tools are used.

use crate::shell::quote;

/// Root directory for worktrees, relative to `$HOME`.
pub const ROOT: &str = ".acc/w";

/// Exit code of [`add_script`] when the directory isn't in a git
/// repository, or the base isn't a commit.
pub const EXIT_NO_REPO: u32 = 6;
/// Exit code of [`merge_script`] when the merge conflicts. Nothing was
/// merged; the session's own commit stays on its branch.
pub const EXIT_CONFLICT: u32 = 7;
/// Exit code of [`diff_script`], [`merge_script`] and [`discard_script`]
/// when there is no such worktree.
pub const EXIT_NO_WORKTREE: u32 = 9;

/// Whether `name` can name a worktree: `[A-Za-z0-9_-]{1,64}`, like
/// session ids.
pub fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The branch a worktree's changes are on.
pub fn branch(name: &str) -> String {
    format!("acc/{name}")
}

fn paths(name: &str) -> String {
    format!(
        "wt=\"$HOME\"/{ROOT}/{name}\n\
         m=\"$wt.meta\"\n"
    )
}

const NEED_META: &str = concat!(
    "[ -f \"$m\" ] || { echo 'acc: no such worktree' >&2; exit ",
    9,
    "; }\n"
);
const _: () = assert!(EXIT_NO_WORKTREE == 9);

/// Commits without failing on a host whose git has no identity configured.
const IDENTITY: &str =
    "git config user.email >/dev/null 2>&1 && id= || id='-c user.name=ACC -c user.email=acc@localhost'\n";

/// Script that makes worktree `name` for the repository `cwd` is in,
/// starting at `base` (a commit-ish; `HEAD` when `None`), and prints the
/// directory matching `cwd` inside it. A worktree that already exists is
/// reused, so relaunching a session lands in the same files. Exits
/// [`EXIT_NO_REPO`] when `cwd` isn't in a repository. `name` must pass
/// [`valid_name`].
pub fn add_script(cwd: &str, name: &str, base: Option<&str>) -> String {
    let base = quote(&format!("{}^{{commit}}", base.unwrap_or("HEAD")));
    let branch = branch(name);
    format!(
        "{paths}\
         cd {cwd} || exit {EXIT_NO_REPO}\n\
         top=$(git rev-parse --show-toplevel 2>/dev/null) || {{ echo 'acc: not in a git repository' >&2; exit {EXIT_NO_REPO}; }}\n\
         prefix=$(git rev-parse --show-prefix)\n\
         if [ ! -d \"$wt\" ]; then\n\
         base=$(git rev-parse -q --verify {base}) || {{ echo 'acc: no such commit' >&2; exit {EXIT_NO_REPO}; }}\n\
         mkdir -p \"$HOME\"/{ROOT}\n\
         if git show-ref -q --verify refs/heads/{branch}; then\n\
         git worktree add \"$wt\" {branch} >&2 || exit 1\n\
         else\n\
         git worktree add -b {branch} \"$wt\" \"$base\" >&2 || exit 1\n\
         fi\n\
         printf '%s\\n%s\\n%s\\n' \"$base\" \"$top\" {branch} >\"$m\"\n\
         fi\n\
         cd \"$wt/$prefix\" 2>/dev/null || cd \"$wt\" || exit 1\n\
         pwd -P\n",
        paths = paths(name),
        cwd = crate::detached::quote_path(cwd),
    )
}

/// Script printing everything the session changed in worktree `name` as a
/// `git diff` (commits it made, edits not committed yet and new files),
/// for [`crate::item::parse_unified`].
pub fn diff_script(name: &str) -> String {
    format!(
        "{paths}{NEED_META}\
         base=$(sed -n 1p \"$m\")\n\
         cd \"$wt\" || exit {EXIT_NO_WORKTREE}\n\
         git diff --no-color --no-ext-diff -M \"$base\" --\n\
         git ls-files --others --exclude-standard | while IFS= read -r f; do\n\
         git diff --no-color --no-ext-diff --no-index -- /dev/null \"$f\"\n\
         done\n\
         true\n",
        paths = paths(name),
    )
}

/// Script that commits what the session changed in worktree `name` and
/// merges it into the branch checked out in the repository (a squash
/// commit with `squash`, else a merge). Prints the new commit. The
/// worktree stays, and later diffs count from here. On a conflict nothing
/// is merged and it exits [`EXIT_CONFLICT`].
pub fn merge_script(name: &str, message: &str, squash: bool) -> String {
    let msg = quote(message);
    let merge = if squash {
        format!(
            "git merge --squash \"$br\" >&2 || {{ git reset -q --merge; exit {EXIT_CONFLICT}; }}\n\
             git diff --cached --quiet || git $id commit -q -m {msg} || exit 1\n"
        )
    } else {
        format!(
            "git $id merge --no-edit -m {msg} \"$br\" >&2 || {{ git merge --abort 2>/dev/null; git reset -q --merge; exit {EXIT_CONFLICT}; }}\n"
        )
    };
    format!(
        "{paths}{NEED_META}\
         top=$(sed -n 2p \"$m\")\n\
         br=$(sed -n 3p \"$m\")\n\
         cd \"$wt\" || exit {EXIT_NO_WORKTREE}\n\
         {IDENTITY}\
         git add -A || exit 1\n\
         git diff --cached --quiet || git $id commit -q -m {msg} || exit 1\n\
         head=$(git rev-parse HEAD)\n\
         cd \"$top\" || exit 1\n\
         {merge}\
         printf '%s\\n%s\\n%s\\n' \"$head\" \"$top\" \"$br\" >\"$m\"\n\
         git rev-parse HEAD\n",
        paths = paths(name),
    )
}

/// Script that deletes worktree `name` and its branch, with every change
/// in it. Stop the session first.
pub fn discard_script(name: &str) -> String {
    format!(
        "{paths}{NEED_META}\
         top=$(sed -n 2p \"$m\")\n\
         br=$(sed -n 3p \"$m\")\n\
         git -C \"$top\" worktree remove --force \"$wt\" 2>/dev/null || rm -rf \"$wt\"\n\
         git -C \"$top\" worktree prune 2>/dev/null\n\
         git -C \"$top\" branch -D \"$br\" >/dev/null 2>&1\n\
         rm -f \"$m\"\n",
        paths = paths(name),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_checked_and_quoted() {
        assert!(valid_name("claude-abc_1"));
        assert!(!valid_name("../x") && !valid_name("") && !valid_name("a b"));
        let s = add_script("~/my repo", "s1", Some("main"));
        assert!(s.contains("\"$HOME\"/'my repo'"), "{s}");
        assert!(s.contains("'main^{commit}'"), "{s}");
        assert!(merge_script("s1", "it's done", false).contains("'it'\\''s done'"));
    }
}

/// The scripts against a real `git`. Unix only.
#[cfg(all(test, unix))]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod git_tests {
    use super::*;
    use crate::item::{FileChangeKind, parse_unified};
    use std::path::Path;
    use std::process::Command;

    fn sh(home: &Path, script: &str) -> (String, Option<i32>) {
        let out = Command::new("sh")
            .arg("-c")
            .arg(script)
            .env("HOME", home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .expect("sh");
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            out.status.code(),
        )
    }

    fn repo(home: &Path) -> std::path::PathBuf {
        let r = home.join("proj");
        std::fs::create_dir_all(r.join("sub")).unwrap();
        std::fs::write(r.join("sub/a.txt"), "one\n").unwrap();
        std::fs::write(r.join("b.txt"), "keep\n").unwrap();
        let (_, code) = sh(
            home,
            &format!(
                "cd {} && git init -q -b main && git add -A && \
                 git -c user.name=t -c user.email=t@t commit -q -m init",
                r.display()
            ),
        );
        assert_eq!(code, Some(0));
        r
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let home = std::env::temp_dir().join(format!("acc-wt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        home.canonicalize().unwrap()
    }

    #[test]
    fn add_diff_merge_and_discard() {
        let home = tmp("flow");
        let r = repo(&home);
        let sub = r.join("sub");
        let (out, code) = sh(&home, &add_script(&sub.to_string_lossy(), "s1", None));
        assert_eq!(code, Some(0));
        let wt = home.join(ROOT).join("s1");
        assert_eq!(out.trim(), wt.join("sub").to_string_lossy());
        // Adding again reuses it.
        assert_eq!(sh(&home, &add_script(&sub.to_string_lossy(), "s1", None)).0, out);

        // The agent edits, adds and commits; the user's checkout is untouched.
        std::fs::write(wt.join("sub/a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(wt.join("new.txt"), "fresh\n").unwrap();
        std::fs::remove_file(wt.join("b.txt")).unwrap();
        assert_eq!(std::fs::read_to_string(r.join("sub/a.txt")).unwrap(), "one\n");
        let (diff, code) = sh(&home, &diff_script("s1"));
        assert_eq!(code, Some(0));
        let mut files: Vec<_> = parse_unified(&diff, "")
            .into_iter()
            .map(|f| (f.path, f.kind))
            .collect();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            files,
            [
                ("b.txt".to_owned(), FileChangeKind::Delete),
                ("new.txt".to_owned(), FileChangeKind::Add),
                ("sub/a.txt".to_owned(), FileChangeKind::Update),
            ]
        );

        let (head, code) = sh(&home, &merge_script("s1", "agent work", false));
        assert_eq!(code, Some(0), "{head}");
        assert_eq!(
            std::fs::read_to_string(r.join("sub/a.txt")).unwrap(),
            "one\ntwo\n"
        );
        assert!(r.join("new.txt").exists() && !r.join("b.txt").exists());
        // Later diffs count from the merge.
        assert_eq!(sh(&home, &diff_script("s1")).0, "");

        sh(&home, &discard_script("s1"));
        assert!(!wt.exists());
        let (branches, _) = sh(&home, &format!("git -C {} branch --list 'acc/*'", r.display()));
        assert!(branches.trim().is_empty(), "{branches}");
        assert_eq!(sh(&home, &diff_script("s1")).1, Some(EXIT_NO_WORKTREE as i32));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn conflicts_merge_nothing_and_squash_makes_one_commit() {
        let home = tmp("conflict");
        let r = repo(&home);
        let dir = r.to_string_lossy().into_owned();
        sh(&home, &add_script(&dir, "c1", None));
        sh(&home, &add_script(&dir, "c2", None));
        let wt = home.join(ROOT);
        std::fs::write(wt.join("c1/b.txt"), "theirs\n").unwrap();
        std::fs::write(wt.join("c2/b.txt"), "mine\n").unwrap();
        assert_eq!(sh(&home, &merge_script("c1", "c1 work", true)).1, Some(0));
        let (log, _) = sh(&home, &format!("git -C {dir} log --format=%s"));
        assert_eq!(log, "c1 work\ninit\n");
        assert_eq!(
            sh(&home, &merge_script("c2", "c2 work", false)).1,
            Some(EXIT_CONFLICT as i32)
        );
        assert_eq!(std::fs::read_to_string(r.join("b.txt")).unwrap(), "theirs\n");
        let (status, _) = sh(&home, &format!("git -C {dir} status --porcelain"));
        assert!(status.trim().is_empty(), "{status}");

        assert_eq!(
            sh(&home, &add_script(&home.to_string_lossy(), "x", None)).1,
            Some(EXIT_NO_REPO as i32)
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}
