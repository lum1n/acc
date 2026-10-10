//! Co-driving a harness TUI in a pane: ACC commands as keystrokes.
//!
//! When a harness runs interactively in a multiplexer, ACC types into its
//! pane like the user would. This is per harness, and only covers what each
//! TUI does reliably from its prompt: submitting text, interrupting, built-in
//! slash commands and exiting. Answering approval dialogs needs the rendered
//! screen and is left to per-harness screen matching.
//!
//! pi is covered fully (checked against pi 1.1.0's keybindings and slash
//! commands). Other harnesses get prompts and the commands whose keys are
//! well known; the rest return [`DriverError::Unsupported`].

use crate::command::{Command, OptionId};
use crate::driver::DriverError;
use crate::harness::Harness;

use super::{Input, Key, Program};

/// Inputs that submit `text` at the TUI's prompt: typed when it is one line,
/// a bracketed paste otherwise, then Enter.
pub fn submit(text: &str) -> Vec<Input> {
    let body = if text.contains(['\n', '\r']) {
        Input::Paste(text.to_owned())
    } else {
        Input::Text(text.to_owned())
    };
    vec![body, Input::Keys(vec![Key::Enter])]
}

fn unsupported(harness: Harness, what: &'static str) -> DriverError {
    DriverError::Unsupported { harness, what }
}

/// Keystrokes that carry out `cmd` in `harness`'s TUI.
pub fn inputs(harness: Harness, cmd: &Command) -> Result<Vec<Input>, DriverError> {
    let slash = |s: String| Ok(submit(&s));
    let with_arg = |name: &str, arg: &str| {
        if arg.is_empty() {
            format!("/{name}")
        } else {
            format!("/{name} {arg}")
        }
    };
    match (harness, cmd) {
        (_, Command::Prompt { text }) => Ok(submit(text)),
        (_, Command::Invoke { name, args }) => slash(with_arg(name.trim_start_matches('/'), args)),
        (Harness::Pi | Harness::Claude | Harness::Codex, Command::Interrupt) => {
            Ok(vec![Input::Keys(vec![Key::Escape])])
        }
        (Harness::Pi | Harness::Claude | Harness::Codex, Command::Compact { hint }) => {
            slash(with_arg("compact", hint.as_deref().unwrap_or("")))
        }
        (Harness::Pi | Harness::Codex, Command::New) => slash("/new".into()),
        (Harness::Claude, Command::New) => slash("/clear".into()),
        (Harness::Pi, Command::Fork) => slash("/clone".into()),
        (Harness::Pi, Command::Rename { name }) => slash(with_arg("name", name)),
        (Harness::Pi | Harness::Claude, Command::Shell { command }) => slash(format!("!{command}")),
        (
            Harness::Pi,
            Command::SetOption {
                id: OptionId::Model,
                value,
            },
        ) => slash(with_arg("model", value)),
        (
            Harness::Pi,
            Command::SetOption {
                id: OptionId::Effort,
                value,
            },
        ) => slash(with_arg("thinking", value)),
        (
            Harness::Claude,
            Command::SetOption {
                id: OptionId::Model,
                value,
            },
        ) => slash(with_arg("model", value)),
        (_, Command::AnswerPermission { .. }) => Err(unsupported(
            harness,
            "answering dialogs without reading the screen",
        )),
        (h, Command::Interrupt) => Err(unsupported(h, "interrupt from a pane")),
        (h, _) => Err(unsupported(h, "this command from a pane")),
    }
}

/// Keystrokes that close the TUI cleanly (for takeover: close it, then
/// resume the session headless). Interrupt a running turn first.
pub fn exit_inputs(harness: Harness) -> Result<Vec<Input>, DriverError> {
    match harness {
        Harness::Pi | Harness::Codex => Ok(submit("/quit")),
        Harness::Claude => Ok(submit("/exit")),
        h => Err(unsupported(h, "a known exit command")),
    }
}

/// The interactive TUI for `harness`, resuming `resume` when given. pi gets
/// `PI_PROGRAM_STATUS=1`, so it reports OSC 7501 through tmux and screen,
/// which never answer the probe.
pub fn launch(harness: Harness, resume: Option<&str>) -> Result<Program, DriverError> {
    let mut program = Program {
        program: harness.program().to_owned(),
        args: Vec::new(),
        env: Vec::new(),
    };
    match (harness, resume) {
        (Harness::Pi, r) => {
            program.env.push(("PI_PROGRAM_STATUS".into(), "1".into()));
            if let Some(path) = r {
                program.args = vec!["--session".into(), path.into()];
            }
        }
        (Harness::Claude, Some(id)) => program.args = vec!["--resume".into(), id.into()],
        (Harness::Codex, Some(id)) => program.args = vec!["resume".into(), id.into()],
        (_, None) => {}
        (h, Some(_)) => return Err(unsupported(h, "resuming a TUI session by id")),
    }
    Ok(program)
}

/// Keystrokes that hand a session back to its TUI in the user's own pane,
/// after ACC closed the TUI there: types the resume command into the shell
/// left in the pane.
pub fn resume_inputs(harness: Harness, resume: &str) -> Result<Vec<Input>, DriverError> {
    let argv = launch(harness, Some(resume))?.argv();
    Ok(submit(&crate::shell::join(argv.iter().map(String::as_str))))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn prompts_type_or_paste() {
        assert_eq!(
            inputs(Harness::Cursor, &Command::Prompt { text: "hi".into() }).unwrap(),
            [Input::Text("hi".into()), Input::Keys(vec![Key::Enter])]
        );
        assert!(matches!(
            inputs(Harness::Pi, &Command::Prompt { text: "a\nb".into() }).unwrap()[0],
            Input::Paste(_)
        ));
    }

    #[test]
    fn pi_commands_are_slash_commands() {
        let first = |c: Command| inputs(Harness::Pi, &c).unwrap().remove(0);
        assert_eq!(
            first(Command::Compact { hint: None }),
            Input::Text("/compact".into())
        );
        assert_eq!(
            first(Command::Rename { name: "x y".into() }),
            Input::Text("/name x y".into())
        );
        assert_eq!(first(Command::Fork), Input::Text("/clone".into()));
        assert_eq!(
            first(Command::Shell { command: "ls".into() }),
            Input::Text("!ls".into())
        );
        assert_eq!(first(Command::Interrupt), Input::Keys(vec![Key::Escape]));
        assert!(inputs(Harness::Pi, &Command::Review).is_err());
    }

    #[test]
    fn launch_and_hand_back() {
        let p = launch(Harness::Pi, Some("/s/x.jsonl")).unwrap();
        assert_eq!(
            p.argv(),
            ["env", "PI_PROGRAM_STATUS=1", "pi", "--session", "/s/x.jsonl"]
        );
        assert_eq!(
            resume_inputs(Harness::Claude, "abc").unwrap()[0],
            Input::Text("claude --resume abc".into())
        );
        assert!(launch(Harness::Cursor, Some("x")).is_err());
        assert_eq!(exit_inputs(Harness::Pi).unwrap()[0], Input::Text("/quit".into()));
    }
}
