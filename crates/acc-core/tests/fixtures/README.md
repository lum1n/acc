# Recorded harness sessions

Real stdout captured from the harnesses on 2026-10-10, replayed by the driver tests.

| File | Harness | What it covers |
|---|---|---|
| `claude/hi-and-controls.ndjson` | Claude Code 2.1.296 | `initialize`, one streamed turn, `rate_limit_event`, `get_context_usage`, `set_model` |
| `claude/permission-deny.ndjson` | Claude Code 2.1.296 | `can_use_tool` for Bash answered with deny, session-state frames |
| `claude/transcript.jsonl` | Claude Code 2.1.296 | the `~/.claude/projects` transcript of both sessions |
| `codex/handshake.ndjson` | codex-cli 0.162.1 | `initialize`, `thread/start`, `model/list`, profiles, modes, rate limits without auth |
| `codex/turn-network-error.ndjson` | codex-cli 0.162.1 | a turn that can't reach the API (retries) |
| `codex/rollout.jsonl` | codex-cli 0.162.1 | the rollout file of that interrupted turn |
| `screens/claude-*.txt` | Claude Code 2.1.296 | `tmux capture-pane -p` of the TUI: Bash and Write permission dialogs (also at 50 columns), an AskUserQuestion dialog, the folder trust dialog, working and idle |
| `screens/pi-*.txt` | pi 0.82.1, 1.1.0 | the TUI in tmux, working and idle (0.82.1 sends no OSC 7501) |

Edits made when recording: home paths and the Claude session id were replaced, very long lists
(tools, commands, skills, models, context grid) were cut down, environment-specific bookkeeping
lines were dropped from the transcript files, and Codex response ids were renumbered to match the
order `CodexDriver::handshake` sends its requests in.
