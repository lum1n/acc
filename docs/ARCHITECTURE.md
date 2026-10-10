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
   transports (outside the core): local process · SSH exec channel ·
   detached FIFO/log · multiplexer pane (tmux control mode, zellij, screen, Rex)
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

## Daemonless sessions

`acc_core::detached` generates host-side shell scripts: the harness runs under `nohup` with stdin on
a FIFO opened read-write (no EOF when a client leaves) and stdout appended to `out.ndjson`. Clients
attach with `tail -c +OFFSET -f` and write through a `mkdir` lock (FIFO writes are only atomic up to
512 bytes on macOS). Disconnects lose nothing, and several clients (desktop and phone) can share one
session.

**Takeover** of a session ACC didn't start: find the process, map it to the harness's session id,
wait for the turn to end (or force), close the TUI, resume headless detached, auto-continue if a turn
was interrupted. Inside a multiplexer ACC can also co-drive the TUI via the multiplexer's CLI.

## State

`RunState` uses OSC 7501's vocabulary (idle, working, blocked{permission,question,auth}, done,
error) plus `exited`. Observations are ranked by source (`Native` > `Osc7501` > `Transcript` >
`ScreenMatch` > `Heuristic`); a lower source only overrides a higher one once it is stale.

## Research

The design decisions come from the project's research notes: core language and harness survey,
daemonless SSH control and takeover, state and usage monitoring, commands and options, and
multiplexer control.
