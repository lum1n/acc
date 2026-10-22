# Recorded harness sessions

Real stdout captured from the harnesses on 2026-10-10 (2026-10-12 for the subagent recordings, 2026-10-13 for the background-agent one, 2026-10-14 for Cursor and the Codex subagent),
replayed by the driver tests.

| File | Harness | What it covers |
|---|---|---|
| `claude/hi-and-controls.ndjson` | Claude Code 2.1.296 | `initialize`, one streamed turn, `rate_limit_event`, `get_context_usage`, `set_model` |
| `claude/permission-deny.ndjson` | Claude Code 2.1.296 | `can_use_tool` for Bash answered with deny, session-state frames |
| `claude/subagents.ndjson` | Claude Code 2.1.280 | `--forward-subagent-text`: a foreground and a background `Agent` call, `task_started`/`task_progress`/`task_updated`/`task_notification`, the background one's hand-back as a queued `<task-notification>` |
| `claude/background-agent-wakes.ndjson` | Claude Code 2.1.280 | a turn that ends while its background `Agent` call runs, then the second turn its report starts (no `running` state), then `idle`; `stream_event` and `thinking_tokens` lines dropped |
| `codex/handshake.ndjson` | codex-cli 0.162.1 | `initialize`, `thread/start`, `model/list`, profiles, modes, rate limits without auth |
| `codex/turn-network-error.ndjson` | codex-cli 0.162.1 | a turn that can't reach the API (retries) |
| `acp/cursor-2026.10.01.{in,out}.ndjson` | Cursor Agent 2026.10.01 | `acc acp cursor` with its default model: `Run echo acc-live-test`, the shell call approved with `allow-once`, no usage reported |
| `codex/subagent-0.162.1.ndjson` | codex-cli 0.162.1 | `acc run codex`: the parent spawns one subagent (`subAgentActivity` `started`), waits for it (`collabAgentToolCall` `wait`), the child's reply streams on its own thread, `subAgentActivity` `completed`, then the parent's answer |
| `acp/opencode-2.0.14-subagent.{in,out}.ndjson` | opencode 2.0.14 | a `subagent` tool call to `explore`, whose child session streams under the parent's session id tagged `_meta["opencode/child-session"]` |

Edits made when recording: home paths and the Claude session id were replaced, very long lists
(tools, commands, skills, models, context grid) were cut down (Cursor's commands to its own, without the recording machine's skills), environment-specific bookkeeping
lines were dropped, and Codex response ids were renumbered to match the
order `CodexDriver::handshake` sends its requests in. The last line of
`acp/opencode-1.18.35.in.ndjson`, the `session/list` ACC sends after a turn to learn opencode's
name for the session, was added by hand when the driver started sending it (2026-10-23).

Codex 0.162 recordings: `skills/list` was cut to two system skills and MCP startup lines were
dropped. The older Codex collab test (`spawnAgent` with
`thread/started`) is still built from the 0.128 app-server TypeScript types and says so in its doc
comment.
