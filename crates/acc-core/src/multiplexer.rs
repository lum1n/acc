//! Locating harness processes inside terminal multiplexers.
//!
//! Each multiplexer leaves identifying variables in the environment of the
//! processes it runs (see `research/multiplexer-control.md`). ACC reads the
//! harness process's environment over SSH and maps it to a pane it can drive
//! with the multiplexer's own CLI.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Multiplexer {
    Tmux,
    Zellij,
    Screen,
    Rex,
}

/// Where a harness runs inside a multiplexer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneRef {
    pub multiplexer: Multiplexer,
    /// tmux socket path, zellij/screen session name, or Rex server.
    pub server: String,
    /// Session name where the multiplexer has one.
    pub session: Option<String>,
    /// tmux `%N`, zellij `terminal_N`, screen window number, Rex block id.
    pub pane: String,
}

/// Identifies the multiplexer from a process environment.
pub fn locate<'a>(env: impl IntoIterator<Item = (&'a str, &'a str)>) -> Option<PaneRef> {
    let mut vars = std::collections::HashMap::new();
    for (k, v) in env {
        vars.insert(k, v);
    }
    let get = |k: &str| vars.get(k).map(|v| (*v).to_owned());

    if let (Some(tmux), Some(pane)) = (get("TMUX"), get("TMUX_PANE")) {
        // TMUX=<socket>,<server pid>,<session index>
        let socket = tmux.split(',').next().unwrap_or_default().to_owned();
        return Some(PaneRef {
            multiplexer: Multiplexer::Tmux,
            server: socket,
            session: None,
            pane,
        });
    }
    if let (Some(session), Some(pane)) = (get("ZELLIJ_SESSION_NAME"), get("ZELLIJ_PANE_ID")) {
        return Some(PaneRef {
            multiplexer: Multiplexer::Zellij,
            server: session.clone(),
            session: Some(session),
            pane: format!("terminal_{pane}"),
        });
    }
    if let (Some(sty), Some(window)) = (get("STY"), get("WINDOW")) {
        return Some(PaneRef {
            multiplexer: Multiplexer::Screen,
            server: sty.clone(),
            session: Some(sty),
            pane: window,
        });
    }
    if let (Some(server), Some(block)) = (get("REX_SERVER"), get("REX_BLOCK")) {
        return Some(PaneRef {
            multiplexer: Multiplexer::Rex,
            server,
            session: get("REX_SESSION"),
            pane: block,
        });
    }
    None
}

/// Parses a NUL-separated environment block such as `/proc/<pid>/environ`.
pub fn parse_environ(block: &str) -> Vec<(&str, &str)> {
    block.split('\0').filter_map(|kv| kv.split_once('=')).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmux_from_environ() {
        let env = "HOME=/h\0TMUX=/tmp/tmux-501/default,1234,0\0TMUX_PANE=%3\0";
        let p = locate(parse_environ(env)).expect("tmux pane");
        assert_eq!(p.multiplexer, Multiplexer::Tmux);
        assert_eq!(p.server, "/tmp/tmux-501/default");
        assert_eq!(p.pane, "%3");
    }

    #[test]
    fn zellij_screen_rex() {
        let z = locate([
            ("ZELLIJ", "0"),
            ("ZELLIJ_SESSION_NAME", "dev"),
            ("ZELLIJ_PANE_ID", "2"),
        ]);
        assert_eq!(z.map(|p| p.pane), Some("terminal_2".into()));
        let s = locate([("STY", "123.work"), ("WINDOW", "1")]);
        assert_eq!(s.map(|p| p.multiplexer), Some(Multiplexer::Screen));
        let r = locate([("REX_SERVER", "local"), ("REX_SESSION", "a"), ("REX_BLOCK", "b7")]);
        assert_eq!(r.map(|p| p.pane), Some("b7".into()));
    }

    #[test]
    fn plain_terminal_is_none() {
        assert_eq!(locate([("TERM", "xterm-ghostty")]), None);
    }
}
