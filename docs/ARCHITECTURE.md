# Architecture

## Goals and constraints

- Control, observe and configure **CLI/TUI** agent harnesses: Claude Code, Codex, Cursor Agent,
  Copilot CLI, pi, opencode. IDE integrations are out of scope for now.
- Embed natively on iOS and Android, and run on web and desktop. **Performance and stability** are
  the top criteria.
- **Install nothing on hosts.** A phone (for example an iOS SSH app embedding ACC) must be able to
  start, observe, control and take over sessions over plain SSH.
- Usage data comes only from what each harness reports itself (tokens, cost, context, quota where
  available). ACC makes no calls to provider usage endpoints.

## Shape

```
            acc-core (Rust, sans-IO, no async runtime)
 ┌──────────────────────────────────────────────────────────────────┐
 │ harness   state (OSC 7501 vocabulary, ranked sources)   event     │
 │ command/options   driver trait   ndjson   osc7501   multiplexer   │
 │ detached (FIFO + log scripts)    shell (quoting)                  │
 └──────────────┬──────────────────────────────┬────────────────────┘
                │ bytes                        │ bytes
   transports (outside the core): local process · acc-ssh (SSH exec channel
   to a detached FIFO/log) · multiplexer pane (tmux control mode, zellij, screen, Rex)
                │
   bindings: acc-ffi (Swift, Kotlin) · acc-wasm (JS) · acc-cli
```

**Sans-IO.** Drivers are pure state machines: harness bytes in, `Event`s out; `Command`s in, bytes
out. The core never spawns, connects or sleeps, so it behaves identically on every platform and
every protocol is tested by replaying recorded sessions.

## Driving harnesses

| Harness | Primary protocol | Fallback |
|---|---|---|
| Claude Code | `-p --input-format stream-json --output-format stream-json` | — |
| Codex | `app-server` JSON-RPC | `exec --json` |
| Cursor Agent | ACP (`agent acp`) | `-p --output-format stream-json` |
| Copilot CLI | ACP (`--acp`) | SDK protocol |
| pi | `--mode rpc` | — |
| opencode | `serve` HTTP + SSE | ACP (`opencode acp`) |

Drivers pin the harness versions they support and fall back to ACP or observe-only for unknown
versions. Unknown events are kept as `Event::Unrecognized`, never fatal.

### ACP driver (Cursor, Copilot, opencode)

`acc_core::acp::AcpDriver` speaks ACP protocol version 1 (`@agentclientprotocol/sdk` 1.8.0). It
sends `initialize`, then opens the session itself (`session/new`, or `session/load` / `session/resume`
for takeover), so transports write `Driver::poll_transmit()` after every `on_output`. Options come from
the agent's `configOptions` (categories `model`, `mode`, `thought_level`, Copilot's `allow_all`), with the
legacy `modes`/`models` fields and `session/set_mode`/`session/set_model` as fallback. Cursor's model is a
spawn flag (`agent --model X acp`), reported as `Apply::Restart`. Slash commands are prompt text, taken
from `available_commands_update`. Request ids carry a per-client tag, and `Start::Attach` lets a second
client (or a reconnect) adopt a running agent from its log, reading other clients' responses by shape.
`acc acp <harness> <cwd>` drives one locally for spikes. Recorded Copilot 1.0.95 and opencode 1.18.35
sessions under `crates/acc-core/tests/fixtures/acp` are replayed in CI.

## Bindings

`acc_core::session::Session` is what every binding wraps: it picks the driver for a harness
(`SessionConfig { harness, cwd, opening: New | Resume | Attach, model, client_tag }`), stamps state
changes with the caller's clock, merges state from other sources (`observe`, e.g. OSC 7501 from a
pane) and remembers the harness's session id. It stays sans-IO, so the app owns the transport:

```
spawn launch_spec() on the host  →  write start()
for each chunk read:  receive(bytes, now_ms) → { events, transmit }  →  write transmit
for each UI action:   send(command) → bytes  →  write them
on EOF:               exited(code, now_ms)
```

| Binding | Crate | Types |
|---|---|---|
| Swift, Kotlin | `acc-ffi` (UniFFI 0.32) | Typed records and enums mirroring the core; harness-native payloads as JSON strings (`extensions_json`, `payload_json`). `Event.UsageUpdated` / `QuotaUpdated` are the core's `Usage` / `Quota` events. |
| JS (browser, Node) | `acc-wasm` (wasm-bindgen) | Plain objects in the core's serde shape (snake_case, `type`-tagged), so they match `acc run --json`; bytes are `Uint8Array`; TypeScript declarations ship in the generated `.d.ts`. |

Both also expose `TranscriptObserver` (follow a Claude, Codex or pi transcript), `DetachedSession`
(the daemonless scripts below), `Osc7501Scanner` and `shell_quote`. CI replays a recorded Claude
session through the real library from Swift (macOS), Kotlin (JVM), Python and Node.

`acc-ffi` also carries the SSH transport (`acc-ssh`), so a phone app needs nothing else to drive a
host end to end. These calls do I/O and are `async` in Swift and `suspend` in Kotlin; they run on a
tokio runtime the library starts on first use (UniFFI's `tokio` support).

- `SshHost(config)`: password, private key or SSH agent; host keys pinned by fingerprint, read from
  `known_hosts`, or accepted for trust on first use (`hostKeyFingerprint()`). `connect`,
  `reconnect` (on returning to the foreground), `disconnect`, `exec`.
- `launchSession` / `attachSession` → `RemoteSession`: the core `Session` on a detached session
  (`nextEvents`, `send`, `stop`, `state`, `commands`, `options`). `listSessions`, `removeSession`.
- `listPanes`, `locatePane(pid)`, `spawnPane`, `paneSession` → `PaneSession`: a harness TUI in a
  tmux, zellij, screen or Rex pane (`nextEvents`, `send`, `sendInput`, `capture`, `exit`, `kill`).

Errors are typed (`Timeout` and `Disconnected` mean calling again retries; `HostKeyRejected` carries
the fingerprint to show). Kotlin needs `kotlinx-coroutines-core` for the suspend calls. A Rust test
drives this surface against a real sshd and tmux; the Swift, Kotlin and Python smoke tests call it
through the generated bindings.

## Daemonless sessions

`acc_core::detached` generates host-side shell scripts: the harness runs under `nohup` with stdin on
a FIFO opened read-write (no EOF when a client leaves) and stdout appended to `out.ndjson`. Clients
attach with `tail -c +OFFSET -f` and write through a `mkdir` lock (FIFO writes are only atomic up to
512 bytes on macOS). Disconnects lose nothing, and several clients (desktop and phone) can share one
session.

### SSH transport (`acc-ssh`)

`acc-ssh` runs those scripts over SSH with [russh](https://docs.rs/russh) (pure Rust, ring crypto, so
it builds for iOS and Android without OpenSSH or CMake; CI builds it for both). Three layers:

- `Host`: one connection, dialed lazily and redialed whenever it has dropped. Every script goes
  through `sh -c`, whatever the login shell (bash, zsh, fish), and launches pick up the `PATH` of the
  user's interactive login shell so nvm and Homebrew installs are found. Host keys are pinned by
  SHA-256 fingerprint, read from `known_hosts`, or accepted for trust on first use. Logins use a
  password, a private key, or an SSH agent (keys and certificates, tried in the agent's order; the
  agent is reached again on every redial).
- `Detached`: the bytes of one session. Reads track the exact log offset and resume there after any
  reconnect, so nothing is lost or repeated. The follower sends a one-byte heartbeat on stderr every
  second; a stream silent for longer than `Retry::stall` (a socket iOS froze in the background) is
  redialed rather than waited out. Writes are uploaded whole into a staging file before they reach the
  FIFO (a dropped upload never leaves half a JSON line) and carry an id, so a retry after a lost
  acknowledgement is not delivered twice. When the harness exits, readers drain the log and report
  the exit code.
- `Session`: the core's `acc_core::session::Session` (driver picked by harness, merged state, native
  session id) on a `Detached`: `send(Command)` and `next_events()`, writing the driver's replies
  itself and stamping state with the clock. A second client attaches from offset 0 and replays the
  session into a fresh core.

`acc_ssh::pane` runs the multiplexer commands below over the same `Host`: list, locate (from a
harness PID's environment), spawn and kill panes, and `Pane`, which co-drives a harness TUI. `Pane`
turns commands into keystrokes (permission answers keyed from the dialog on screen at that moment),
and its `next_events` streams the multiplexer's watch output for OSC 7501 states, reads the screen
when the pane draws (at most every `Pacing::min_gap`, at least every `Pacing::idle`), reopens the
watch after a dropped connection, and reports `Exited` once the pane is gone. A channel that fails
to open on a connection that died unnoticed is retried once on a fresh one, since the script never
started; anything that may have run (a keystroke) is never repeated.

CI drives a real OpenSSH server through a proxy that cuts and freezes connections mid-session, with
tmux behind it for `Pane`.

**Takeover** of a session ACC didn't start: find the process, map it to the harness's session id,
wait for the turn to end (or force), close the TUI, resume headless detached, auto-continue if a turn
was interrupted. Inside a multiplexer ACC can also co-drive the TUI via the multiplexer's CLI.

## Multiplexers

`acc_core::multiplexer` drives harnesses running inside tmux, zellij, screen and Rex with the host's
own binary over SSH exec channels. A `Backend` per multiplexer builds the commands (list panes,
capture, send text/paste/keys, spawn, kill, watch) and parses their output; nothing is installed.

| | Watch stream | OSC 7501 |
|---|---|---|
| tmux 3.2+ | control mode (`tmux -C attach -f ignore-size`), raw `%output` bytes | reports sent without the probe (pi with `PI_PROGRAM_STATUS=1`) |
| zellij 0.44+ | `zellij subscribe -f json`, rendered lines | no |
| screen 4.01+ | window log tailed with `tail -F` (best effort) | as tmux |
| Rex | `rex events --json`, native program-status records | yes, for every harness that emits it |

`multiplexer::tui` turns ACC commands into keystrokes for co-driving a harness TUI in its pane (pi
fully; prompts, interrupt and common slash commands for Claude and Codex). Approval dialogs in a
pane still need per-harness screen matching. The Rex backend follows Rex's docs and is unverified
until Rex ships a server outside the macOS app. Tests drive real tmux, zellij and screen in CI.


`RunState` uses OSC 7501's vocabulary (idle, working, blocked{permission,question,auth}, done,
error) plus `exited`. Observations are ranked by source (`Native` > `Osc7501` > `Transcript` >
`ScreenMatch` > `Heuristic`); a lower source only overrides a higher one once it is stale.

## Research

The design decisions come from the project's research notes: core language and harness survey,
daemonless SSH control and takeover, state and usage monitoring, commands and options, and
multiplexer control.
