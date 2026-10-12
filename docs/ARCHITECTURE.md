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

**Images.** `Command::Prompt` and `LaunchRequest` carry `Attachment`s: images inline as base64, so
nothing is copied to the host first and the bytes ride the same stdin path as text. Every
harness takes them in its prompt: Claude as Anthropic `image` content blocks, Codex as
`{"type":"image","url":"data:..."}` input items (0.128; upstream renames `url` to `image_url`),
ACP as `image` blocks when the agent advertised `promptCapabilities.image` (Cursor, Copilot and
opencode do; otherwise `DriverError::Unsupported`), pi as the prompt's `images`. Harnesses echo
the prompt into the session log, where a line over 16 MiB is dropped, so `validate_attachments`
rejects an image over 5 MiB or a prompt over 10 MiB (`DriverError::AttachmentRejected`); clients
resize first. Transcripts show a prompt's text, or `[image]` when it had none.

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

## Interactive commands

A TUI's interactive slash commands (pickers, dialogs) don't work headless: harnesses refuse them
or send them to the model as text. Drivers map them onto the protocol instead, so palette entries
describe what a client has to draw:

- `CommandEntry::input`: `Option { id }` makes `/model`, `/effort`, `/mode` (and Codex's and ACP's
  `/permissions`) a picker over `Driver::options`; invoking with the chosen value sets the option.
  `Choice` is a fixed list (Claude's output styles). Invoking a picker without a value is
  `DriverError::MissingArgument`, never text for the model.
- `Event::Panel`: the entry runs a protocol request and the driver answers with a dialog (sections
  of rows, each with actions). An action is just `Command::Invoke { name, args }`; afterwards the
  driver fetches the panel again. Panels answer only the client that asked.
- `Exec::Terminal`: nothing on the wire can run it (Claude's `/doctor`); clients open the
  harness's TUI with `tui_command`.
- Commands a TUI runs itself and never lists headless are re-created from protocol calls: Claude's
  `/plan [<description>]` is `set_permission_mode` into plan mode, the description sent as the first
  prompt once Claude confirms, and `get_plan` as a panel when already planning.

| Harness | Pickers | Panels |
|---|---|---|
| Claude | model, effort, mode (permission mode), output-style | mcp (reconnect, enable, disable), status, permissions, memory, hooks, plan (the session plan); reload-plugins |
| Codex | model, effort, mode, permissions | mcp (reload, OAuth login), status, experimental (toggle) |
| Cursor, Copilot, opencode | whatever `configOptions` reports | none: ACP has no such requests |
| pi | model, thinking | session, settings (auto-compaction, queueing), fork (pick a message); export |

## Daemonless sessions

`acc_core::detached` generates host-side shell scripts: the harness runs under `nohup` with stdin on
a FIFO opened read-write (no EOF when a client leaves) and stdout appended to `out.ndjson`. Clients
attach with `tail -c +OFFSET -f` and write through a `mkdir` lock (FIFO writes are only atomic up to
512 bytes on macOS). Disconnects lose nothing, and several clients (desktop and phone) can share one
session.

**Takeover** of a session ACC didn't start: find the process, map it to the harness's session id,
wait for the turn to end (or force), close the TUI, resume headless detached, auto-continue if a turn
was interrupted. Inside a multiplexer ACC can also co-drive the TUI via the multiplexer's CLI.

## Sessions

`acc_core::session::Session` binds a driver to a detached session directory. `launch` writes
`spec.json` (harness, cwd, launch command, native id once known) next to the log, so any client can
list sessions (`detached::list_script`) and attach. Attaching replays `out.ndjson` through a fresh
driver while suppressing everything it would transmit, then sends only safe, repeatable queries
(`Driver::attach_handshake`). Request ids carry a per-client tag, and drivers read other clients'
responses by shape, so several clients share one harness. Claude answers travel on stdin and never
reach the log, so the Claude driver resolves a permission from the tool's result instead; every
client, and every replay, sees it answered.

Every update carries a `replay` flag (history vs. live, for notifications) and the change it made to
the session's `Transcript`: items with accumulated text and merged extensions. `item::detail` turns
each harness's native item JSON into one typed shape (tool call, command with output and exit code,
file change with hunks) and `item::parse_unified` reads `git diff`. Commands a harness can only carry
out with a new process (Claude fork, a model Cursor takes at spawn) come back as
`Outbound::Relaunch`.

`detached::fan_in_script` follows any number of logs and transcripts on one SSH channel, tagged per
file, with byte offsets so a restarted follower resumes without loss; closing the channel takes all
its processes down. One long-lived channel per host keeps a client inside sshd's per-connection
session limit however many agents run.

## Subagents

A tool call that runs another agent (Claude's `Agent`, Codex's collab tools `spawn_agent` /
`send_input` / `wait_agent` / `close_agent` / `resume_agent`, opencode's `task`, pi's `subagent`) is
`ItemKind::Subagent`. Its typed view, `item::ItemDetail::Subagent`, carries the agent type, the task,
the harness's id for the child conversation, its status and its report. Every item of that child
conversation — messages, tool calls, commands — carries the subagent item's id as `Event::ItemStarted`'s
`parent`, so `Transcript::children(id)` is the run's own conversation and `Transcript::top_level()` is
the main one with every subagent folded away; `Transcript::subagents()` lists the runs themselves.

A subagent launched in the background outlives the turn that started it: `Transcript::settle` leaves
it `Running` until it reports back (or the process exits), so a client doesn't show it as finished
early.

| Harness | How a subagent shows up | Child conversation |
|---|---|---|
| Claude | `Agent`/`Task` tool call; `system/task_started`, `task_progress`, `task_updated`, `task_notification` merge onto it; a background launch's tool result only acknowledges (`isAsync`), the notification finishes it | `~/.claude/projects/<proj>/<session>/subagents/agent-<id>.jsonl`, named by `agent-<id>.meta.json`'s `toolUseId`; `Driver::launch` adds `--forward-subagent-text` so a driven session streams it live too |
| Codex | `collabAgentToolCall` item (`spawnAgent`, ...); `thread/started` with `source.subAgent.thread_spawn.parent_thread_id` names the child thread | the child's own thread: its `item/*` notifications carry its `threadId`, mapped back to the spawning item |
| Cursor, Copilot, opencode (ACP) | a `task`/`subagent`-shaped tool call (name, or `rawInput.subagent_type`/`agent` with a prompt); the report is opencode's `task_id: …`/`<task_result>` (1.x) or `<subagent sessionID="…">` (2.0) wrapper, unwrapped by `item::detail` | opencode forwards the child session's updates under the parent's session id, tagged `_meta["opencode/child-session"]`; other agents report only the final text |
| pi | the pi-subagents extension's `subagent` tool | none (no child transcript) |

**Observing** (a TUI ACC didn't start): `Observed::children_exec` finds subagent transcripts not
followed yet — Claude's `subagents_script` next to the session, Codex's rollout `locate_script` for a
spawned thread id — and `Observed::on_children` matches what it finds to the spawning item.
`Observed::child_feed_entries` joins the session's own entry in the client's fan-in; `Observed::feed_child`
routes each child's bytes through its own reader (`claude::transcript::Observer::for_subagent`,
`codex::rollout::Observer::for_child`), whose items carry the spawning item as `parent` and whose state
and usage are the parent's, not reported again.

## Foreign sessions

`discover` lists harness TUIs ACC didn't start: process, working directory, multiplexer pane (from
its environment), and its transcript (an open file, else the newest in the harness's directory).
`observe::Observed` follows that transcript (Claude, Codex and pi observers emit full conversation
items, not just state), infers a waiting approval from a quiet outstanding tool call, and can type
prompts into the pane. `takeover::Takeover` is the state machine for moving it into a detached
session: wait for a safe point (or interrupt when forced), close the TUI the way a user would,
confirm the process is gone, resume headless with the harness's session id. `hand_back_exec` types
the resume command back into the original pane.

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
fully; prompts, interrupt and common slash commands for Claude and Codex; never images, which only
the headless protocols carry). Approval dialogs in a pane still need per-harness screen matching. The Rex backend follows Rex's docs and is unverified
until Rex ships a server outside the macOS app. Tests drive real tmux, zellij and screen in CI.


`RunState` uses OSC 7501's vocabulary (idle, working, blocked{permission,question,auth}, done,
error) plus `exited`. Observations are ranked by source (`Native` > `Osc7501` > `Transcript` >
`ScreenMatch` > `Heuristic`); a lower source only overrides a higher one once it is stale.

## Research

The design decisions come from the project's research notes: core language and harness survey,
daemonless SSH control and takeover, state and usage monitoring, commands and options, and
multiplexer control.
