# Recorded harness sessions

Real stdout captured from the harnesses on 2026-10-10 (2026-10-12 for the subagent recordings),
replayed by the driver tests.

| File | Harness | What it covers |
|---|---|---|
| `claude/hi-and-controls.ndjson` | Claude Code 2.1.296 | `initialize`, one streamed turn, `rate_limit_event`, `get_context_usage`, `set_model` |
| `claude/permission-deny.ndjson` | Claude Code 2.1.296 | `can_use_tool` for Bash answered with deny, session-state frames |
| `claude/transcript.jsonl` | Claude Code 2.1.296 | the `~/.claude/projects` transcript of both sessions |
| `claude/subagents.ndjson` | Claude Code 2.1.280 | `--forward-subagent-text`: a foreground and a background `Agent` call, `task_started`/`task_progress`/`task_updated`/`task_notification`, the background one's hand-back as a queued `<task-notification>` |
| `claude/subagents/<session>.jsonl` and `<session>/subagents/agent-*.{jsonl,meta.json}` | Claude Code 2.1.280 | the `~/.claude/projects` transcript of that session, with its two subagents' own transcripts and meta files |
| `codex/handshake.ndjson` | codex-cli 0.162.1 | `initialize`, `thread/start`, `model/list`, profiles, modes, rate limits without auth |
| `codex/turn-network-error.ndjson` | codex-cli 0.162.1 | a turn that can't reach the API (retries) |
| `codex/rollout.jsonl` | codex-cli 0.162.1 | the rollout file of that interrupted turn |
| `acp/opencode-2.0.14-subagent.{in,out}.ndjson` | opencode 2.0.14 | a `subagent` tool call to `explore`, whose child session streams under the parent's session id tagged `_meta["opencode/child-session"]` |

Edits made when recording: home paths and the Claude session id were replaced, very long lists
(tools, commands, skills, models, context grid) were cut down, environment-specific bookkeeping
lines were dropped from the transcript files, and Codex response ids were renumbered to match the
order `CodexDriver::handshake` sends its requests in.

The Codex collab (subagent) tests aren't recorded: no account was logged in when this was written.
They're built from the 0.128 app-server TypeScript types (`codex app-server generate-ts`) and the
`collab_*` strings in the `codex` binary instead, and are called out as synthetic in their doc
comments.
