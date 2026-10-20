// Types for @acc/node. The core's values are its serde shapes, as in acc-wasm's declarations.

export type HarnessId = "claude" | "codex" | "cursor" | "copilot" | "pi" | "opencode";

export interface HarnessInfo {
  harness: HarnessId;
  id: string;
  program: string;
  display_name: string;
  native_acp: boolean;
}

export interface LaunchSpec {
  program: string;
  args: string[];
  env: [string, string][];
  protocol: "claude_stream_json" | "codex_app_server" | "acp" | "pi_rpc" | "opencode_http";
}

export type RunState =
  | { state: "idle" }
  | { state: "working"; progress: number | null }
  | { state: "blocked"; kind: "permission" | "question" | "auth" }
  | { state: "done" }
  | { state: "error" }
  | { state: "exited" };

export type StateSource = "heuristic" | "screen_match" | "transcript" | "osc7501" | "native";

export interface SessionState {
  state: RunState;
  message: string | null;
  source: StateSource;
  /** Milliseconds since the Unix epoch. */
  observed_at_ms: number;
}

export type ItemKind =
  | "user_message" | "agent_message" | "reasoning" | "tool_call" | "command_execution" | "file_change"
  | "subagent";

export type ItemStatus = "running" | "completed" | "failed";

export type ToolKind = "shell" | "edit" | "read" | "fetch" | "mcp" | "other";

export interface FileDiff {
  path: string;
  old_path: string | null;
  kind: "add" | "delete" | "update" | "rename";
  hunks: {
    old_start: number;
    old_lines: number;
    new_start: number;
    new_lines: number;
    lines: { tag: "context" | "add" | "remove"; text: string }[];
  }[];
  added: number;
  removed: number;
}

/**
 * What an item is, the same shape on every harness. `tool` is the
 * harness's own name for the tool (`Bash`, `commandExecution`, `execute`).
 */
export type ItemDetail =
  | { type: "text" }
  | {
      type: "tool";
      tool: string;
      kind: ToolKind;
      title: string;
      /** The tool's arguments. */
      input: unknown;
      output: string | null;
      paths?: string[];
    }
  | { type: "command"; tool: string; command: string; cwd: string | null; output: string; exit_code: number | null }
  | { type: "file_change"; tool: string; files: FileDiff[] }
  | {
      type: "subagent";
      tool: string;
      agent: string | null;
      description: string;
      prompt: string | null;
      child: string | null;
      status: string | null;
      output: string | null;
      model: string | null;
      /** Runs on after its turn ends, until it reports back. */
      background?: boolean;
    };

export interface Item {
  id: string;
  kind: ItemKind;
  status: ItemStatus;
  text: string;
  detail: ItemDetail;
  /** The harness's own JSON, every frame merged; for debugging only. */
  native: unknown;
  parent: string | null;
  seq: number;
}

export interface Usage {
  input_tokens: number | null;
  output_tokens: number | null;
  cache_read_tokens: number | null;
  cache_write_tokens: number | null;
  cost_usd: number | null;
  context_used: number | null;
  context_window: number | null;
}

export interface QuotaWindow {
  id: string;
  used_percent: number | null;
  window_minutes: number | null;
  resets_at: number | null;
  status: string | null;
}

export interface Quota {
  account: string | null;
  plan: string | null;
  windows: QuotaWindow[];
}

export type Hook = { type: "command"; command: string } | { type: "webhook"; url: string };

/** One event a session follower recorded while no client was connected. */
export interface HostEvent {
  /** Seconds since the Unix epoch, by the host's clock. */
  at: number;
  session: string;
  state: "blocked" | "done" | "error" | "exited";
  kind?: "permission" | "question" | "auth";
  detail?: string;
  code?: number | null;
  /** The session log's length at the time. */
  offset: number;
}

export interface PermissionOption {
  id: string;
  label: string;
  /** What answering with it does, when the harness says. */
  kind?: "allow_once" | "allow_always" | "reject_once" | "reject_always";
}

export interface PolicyRule {
  decision: "allow" | "deny";
  /** The harness's tool name, or "shell" | "edit" | "read" | "fetch" | "mcp". */
  tool?: string | null;
  /** Glob over the files the tool acts on, relative to the session's directory. */
  path?: string | null;
  /** A shell command's start, in whole words. */
  command?: string | null;
}

export interface Spend {
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  cost_usd: number;
}

/** One session's spending, from `AccSession.usageRecord()`. Keep it for sessions that are gone. */
export interface UsageRecord {
  session_id: string;
  harness: HarnessId;
  /** Quarter-hour buckets, oldest first; `start_ms` is ms since the Unix epoch. */
  buckets: { start_ms: number; spend: Spend }[];
  quota?: Quota;
  quota_at_ms?: number;
}

export interface UsageRollup {
  total: Spend;
  accounts: {
    harness: HarnessId;
    account: string | null;
    plan: string | null;
    sessions: number;
    spend: Spend;
    quota: Quota | null;
  }[];
  /** Oldest first; `date` is `YYYY-MM-DD` in the time zone given. */
  days: { date: string; spend: Spend; harnesses: { harness: HarnessId; spend: Spend }[] }[];
}

export type Event =
  | { type: "session_started"; harness: HarnessId; native_id: string; model: string | null }
  | ({ type: "state_changed" } & SessionState)
  | { type: "item_started"; id: string; kind: ItemKind; parent?: string; detail: ItemDetail; native?: unknown }
  | { type: "item_delta"; id: string; text: string }
  | { type: "item_updated"; id: string; detail: ItemDetail; native?: unknown }
  | { type: "item_completed"; id: string; status: ItemStatus; detail: ItemDetail; native?: unknown }
  | {
      type: "permission_requested";
      request_id: string;
      title: string;
      options: PermissionOption[];
      /** A question (any text is an answer) rather than an approval. */
      kind: "permission" | "question" | "auth";
      item_id?: string;
      input?: unknown;
    }
  | { type: "permission_resolved"; request_id: string }
  | ({ type: "usage" } & Usage)
  | ({ type: "quota" } & Quota)
  | { type: "commands_changed" }
  | { type: "options_changed" }
  | { type: "notice"; message: string }
  | { type: "turn_completed" }
  | { type: "exited"; code: number | null }
  | { type: "unrecognized"; raw: string }
  | { type: "native"; kind: string; payload?: unknown };

export type OptionId = "model" | "effort" | "mode" | "permission";

export type Command =
  | { type: "prompt"; text: string }
  | { type: "interrupt" }
  | { type: "compact"; hint?: string | null }
  | { type: "new" }
  | { type: "fork" }
  | { type: "rename"; name: string }
  | { type: "review" }
  | { type: "shell"; command: string }
  | { type: "set_option"; id: OptionId; value: string }
  | { type: "answer_permission"; request_id: string; option_id: string }
  | { type: "invoke"; name: string; args: string }
  | { type: "refresh" }
  | { type: "handoff"; harness: HarnessId; prompt?: string | null };

export interface CommandEntry {
  name: string;
  description: string;
  arg_hint: string | null;
  source: "builtin" | "custom" | "skill" | "mcp" | "plugin" | "acc";
  exec:
    | { exec: "text" }
    | { exec: "rpc"; method: string }
    | { exec: "endpoint"; method: string; path: string }
    | { exec: "skill_item" };
}

export interface OptionDef {
  id: OptionId;
  values: string[];
  current: string | null;
  apply: "live" | "next_turn" | "restart";
}


export type Compat = "tested" | "newer" | "older" | "unknown";

/** A harness found on a host. */
export interface Availability {
  harness: HarnessId;
  path: string;
  version: string | null;
  compat: Compat;
}

export type DetachedStatus =
  | { status: "alive"; pid: number; size: number }
  | { status: "exited"; code: number | null; size: number }
  | { status: "missing" };

/** What a detached session was started with (`~/.acc/s/<id>/spec.json`). */
export interface SessionSpec {
  v: number;
  id: string;
  harness: HarnessId;
  cwd: string;
  launch: LaunchSpec;
  model: string | null;
  permission: string | null;
  resume: string | null;
  fork: boolean;
  native_id: string | null;
  title: string | null;
  created_ms: number;
  parent: string | null;
  labels?: Record<string, string>;
  worktree?: string;
}

/** A detached session on a host. */
export interface DetachedInfo {
  id: string;
  status: DetachedStatus;
  /** The log's last change, in seconds since the Unix epoch. */
  modified: number | null;
  /** `null` for sessions an older ACC started. */
  spec: SessionSpec | null;
}

export interface SessionConfig {
  harness: HarnessId;
  /** Absolute working directory on the host. */
  cwd: string;
  /** The harness's own id of a session to resume. */
  resume?: string | null;
  model?: string | null;
  /** Unique among clients sharing one harness process. Defaults to "acc". */
  client_tag?: string | null;
}

/** Exactly one of `password`, `privateKey` or `agent`. */
export interface SshAuth {
  password?: string;
  /** OpenSSH, PEM or PuTTY format. */
  privateKey?: string;
  passphrase?: string;
  /** Use an SSH agent, at `agentSocket` or `$SSH_AUTH_SOCK`. */
  agent?: boolean;
  agentSocket?: string;
}

/** Exactly one of `fingerprints`, `knownHosts` or `acceptAny`. */
export interface HostKeyCheck {
  /** `SHA256:…`, as `ssh-keygen -lf` prints them. */
  fingerprints?: string[];
  knownHosts?: string;
  /** Trust on first use: connect, read `hostKeyFingerprint()`, pin it. */
  acceptAny?: boolean;
}

export interface SshConfig {
  host: string;
  /** Default 22. */
  port?: number;
  user: string;
  auth: SshAuth;
  hostKey: HostKeyCheck;
  connectTimeoutMs?: number;
  keepaliveIntervalMs?: number;
  keepaliveMax?: number;
}

export interface ExecOutput {
  stdout: Buffer;
  stderr: Buffer;
  /** `null` when the script was killed by a signal. */
  exitCode: number | null;
}

/** Thrown errors carry acc-ffi's `AccError` variant name as `code`. */
export type AccErrorCode =
  | "Timeout" | "Disconnected" | "AuthRejected" | "HostKeyRejected" | "Agent" | "NotRunning"
  | "Remote" | "Driver" | "InvalidArgument" | "Ssh" | "Internal";

/** A host reachable over SSH. Connects on first use and redials whenever the connection dropped. */
export declare class SshHost {
  constructor(config: SshConfig);
  /** Connects now, to surface login and host-key errors early. */
  connect(): Promise<void>;
  reconnect(): Promise<void>;
  /** Closes the connection. Detached sessions keep running. */
  disconnect(): Promise<void>;
  /** SHA-256 fingerprint of the key the host presented on the last dial. */
  hostKeyFingerprint(): string | null;
  exec(script: string, stdin?: Buffer | null): Promise<ExecOutput>;
  /** The harnesses installed on the host. */
  probe(): Promise<Availability[]>;
  /** The detached sessions on the host, from any client. */
  listSessions(): Promise<DetachedInfo[]>;
  /** Starts a harness detached as session `id` (`[A-Za-z0-9_-]{1,64}`). */
  launchSession(id: string, config: SessionConfig): Promise<RemoteSession>;
  /** Joins a running session, replaying its log, then follows it live. */
  attachSession(id: string, config: SessionConfig): RemoteSession;
  /** Deletes an exited session's files. */
  removeSession(id: string): Promise<void>;
}

/** A harness running detached on a host: commands in, events out. */
export declare class RemoteSession {
  readonly id: string;
  readonly harness: HarnessId;
  /** Log bytes delivered so far. */
  readonly offset: number;
  /**
   * Waits for the next events, reconnecting as needed; after the exit, `[exited]` on every call.
   * Keep one caller waiting on it: an abandoned call can drop the events it was about to return.
   */
  nextEvents(): Promise<Event[]>;
  /** Delivered whole and at most once. */
  send(command: Command): Promise<void>;
  /** Asks the harness to exit; `nextEvents` then reports it. */
  stop(): Promise<void>;
  status(): Promise<DetachedStatus>;
  state(): SessionState | null;
  observe(state: SessionState): boolean;
  nativeId(): string | null;
  commands(): CommandEntry[];
  options(): OptionDef[];
  items(): Item[];
  /** Items that didn't run inside a subagent. */
  topLevelItems(): Item[];
}

/** An ed25519 key: `privateKey` for `auth.privateKey`, `publicKey` for `authorized_keys`. */
export declare function generateKey(comment: string): { privateKey: string; publicKey: string };
/** A session id for `harness` (`claude-…`) from at least 15 random bytes. */
export declare function newSessionId(harness: HarnessId, entropy: Buffer): string;
export declare function harnesses(): HarnessInfo[];
