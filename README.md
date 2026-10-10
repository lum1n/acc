# ACC: Agent Control Core

One embeddable Rust core that **controls, observes and configures** the major CLI agent harnesses:
Claude Code, Codex, Cursor Agent, Copilot CLI, pi and opencode.

It ships as a native library for iOS and Android (UniFFI → Swift/Kotlin), the browser and Node
(wasm-bindgen), and desktop (plain Rust). It needs **nothing installed on the hosts** it controls:
everything runs over an SSH exec channel using the harnesses' own headless protocols and standard
POSIX tools.

> Status: skeleton. The core model, parsers and daemonless session scripts are in place and tested;
> harness drivers are next.

## Workspace

| Crate | What it is |
|---|---|
| `crates/acc-core` | Sans-IO core: harness catalogue, session state model, event stream, commands/options, driver trait, NDJSON and OSC 7501 parsers, detached-session scripts, multiplexer detection |
| `crates/acc-ssh` | Daemonless SSH transport (pure-Rust russh): run any driver on a host over plain SSH, surviving dropped connections |
| `crates/acc-ffi` | UniFFI bindings for Swift and Kotlin (`staticlib` + `cdylib`) |
| `crates/acc-wasm` | wasm-bindgen bindings for browsers and Node |
| `crates/acc-cli` | `acc` command-line tool for hosts and spikes |

## Quick start

```sh
cargo test --workspace

# What can ACC drive on this machine?
cargo run -q -p acc-cli -- doctor

# Start Claude Code detached on a host, nothing installed there:
cargo run -q -p acc-cli -- script launch claude s1 ~/project | ssh host sh
cargo run -q -p acc-cli -- script attach s1 | ssh host sh      # follow output
cargo run -q -p acc-cli -- script status s1 | ssh host sh

# Swift / Kotlin bindings
cargo build -p acc-ffi --release
cargo run -p acc-ffi --features bindgen --bin uniffi-bindgen -- \
  generate --library target/release/libacc_ffi.so --language swift --language kotlin --out-dir bindings
```

## Try it by hand

Everything below runs in a plain terminal. Install the CLI once:

```sh
cargo install --path crates/acc-cli    # puts `acc` in ~/.cargo/bin
acc doctor                             # harnesses, multiplexers and session stores found here
```

**Drive a harness.** `acc run <harness> [dir]` starts it headlessly through its ACC driver
(`claude`, `codex`, `pi`, `cursor`, `copilot`, `opencode`). Type prompts; events print as
readable lines (`--json` prints the raw event stream). `:help` lists the commands: `:answer allow`
for a permission prompt, `:interrupt`, `:compact`, `:set model <m>`, `:commands`, `:options`.
Ctrl-D ends the session after the current turn.

```sh
acc run claude ~/project
echo "Reply with: hello from acc" | acc run claude .    # one-shot smoke test
acc run codex . --model gpt-5.5 --json
```

**On another machine, over SSH.** Add `--ssh [user@]host[:port]` and the harness runs detached
on that host, with nothing installed there. Ending input (Ctrl-D) or losing the connection leaves
it running; `--attach` rejoins it from any machine and replays what you missed. Keys come from
`~/.ssh` (or `--identity`), host keys are checked against `~/.ssh/known_hosts`.

```sh
acc run claude ~/project --ssh me@devbox            # prints the id to rejoin with
acc ssh-sessions me@devbox                          # detached sessions on that host
acc run claude --ssh me@devbox --attach <id>        # rejoin, from the laptop or anywhere
acc ssh-sessions me@devbox --stop <id> --prune      # stop one, clean up ended ones
```

**Resume or take over a session.** `acc sessions <claude|codex|pi>` lists recent sessions with
the command that resumes each one. Quit the harness's own TUI first, then resume it headlessly
(Claude resumes per project, so run it from the session's directory):

```sh
acc sessions claude
acc run claude . --resume <session-id>
```

**Inside tmux, zellij or screen.** Leave the harness running in its pane and drive it from any
other terminal; each `acc script mux` command prints a shell script, so pipe it to `sh` (or to
`ssh host sh` for a remote machine):

```sh
acc script mux tmux - - list | sh                      # find the harness's pane, e.g. %3
acc script mux tmux - - capture %3 | sh                # what the pane shows now
acc script mux tmux - - type %3 "fix the tests" | sh   # type a prompt and press Enter
acc script mux zellij - - list | sh                    # zellij: panes of every session
acc script mux zellij <session> - type terminal_1 "fix the tests" | sh
```

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design and the decisions behind it.
