// Types mirroring the serde payloads of the catus-core runtime boundary.
//
// Source of truth:
// - RuntimeEvent: catus-core/src/app.rs (manual Serialize impl)
// - AppSnapshot / SubagentSummary: catus-core/src/app.rs
// - UiRequest / CommandOutcome: catus-core/src/app/command.rs
// - Message: catus-core/src/message.rs
// - AskQuestion / AskAnswer: catus-core/src/tool/ask_user.rs
// - ToolCall: OpenAI function-call shape, catus-core/src/tool.rs

export type Role = 'system' | 'user' | 'assistant' | 'tool' | 'event';

export interface ToolCall {
  id: string;
  type: 'function';
  function: { name: string; arguments: string };
}

export interface Message {
  role: Role;
  content: string;
  reasoning_content?: string;
  tool_call_id?: string;
  had_tool_calls?: boolean;
  tool_calls?: ToolCall[];
}

export interface Usage {
  prompt_tokens: number;
  completion_tokens: number;
  total_tokens: number;
  cached_tokens: number;
}

export interface ModelInfo {
  id: string;
  name: string;
  context_window: number;
  /** The provider is skipped by serde (it carries the api key); only its name. */
  provider_name: string;
}

/** Configuration form of one `[[models]]` entry (editable on the models page). */
export interface ModelEntry {
  id: string;
  name: string;
  /** Token count; the backend also accepts `"128k"`-style strings. */
  context_window: number | string;
  provider: string;
}

export type AppStatus = 'idle' | 'streaming' | 'running_tool' | 'error';

export interface AskOption {
  label: string;
  description?: string;
}

export interface AskQuestion {
  prompt: string;
  title: string;
  options: AskOption[];
  multiSelect: boolean;
}

/** `Answer` is serde-untagged: a single string or a list of strings. */
export interface AskAnswer {
  prompt: string;
  answer: string | string[];
}

export type SubagentState =
  | 'idle'
  | 'streaming'
  | 'running_tool'
  | 'completed'
  | 'error';

export type SubagentEventPayload =
  | { type: 'started'; id: string }
  | { type: 'state_changed'; id: string; state: SubagentState }
  | { type: 'message'; id: string; message: Message }
  | { type: 'completed'; id: string; result: string }
  | { type: 'error'; id: string; error: string };

export type RuntimeEventPayload =
  | { type: 'stream_text'; text: string }
  | { type: 'stream_reasoning'; text: string }
  | { type: 'tool_call_added'; call: ToolCall }
  | { type: 'usage_updated'; usage: Usage }
  | { type: 'messages_changed' }
  | { type: 'interaction_requested'; questions: AskQuestion[] }
  | { type: 'subagent_event'; event: SubagentEventPayload }
  | { type: 'turn_complete' };

export interface TodoItem {
  id: number;
  text: string;
  done: boolean;
}

export interface SubagentSummary {
  id: string;
  name: string;
  task: string;
  state: SubagentState;
  mode: 'fork' | 'create';
  result: string | null;
  error: string | null;
}

export type UiRequest =
  | { page: 'show_status' }
  | { page: 'show_config' }
  | { page: 'show_resume_picker'; items: string[] }
  | { page: 'show_skills'; items: string[] }
  | { page: 'show_models'; items: string[]; selected: number }
  | { page: 'show_agents'; items: string[] }
  | { page: 'show_subagents'; items: string[] }
  | { page: 'watch_subagent'; id: string }
  | { page: 'close_subagent_picker'; items: string[] };

/** Which config file a field is read from / written to. */
export type ConfigScope = 'workspace' | 'global';

/** One config file's editable-field state (empty value = not set there). */
export interface ConfigScopeSnapshot {
  scope: ConfigScope;
  path: string;
  exists: boolean;
  fields: [string, string][];
  /** `[[models]]` entries as written in this scope's file. */
  models: ModelEntry[];
  /** `[[mcp.servers]]` entries as written in this scope's file. */
  mcp_servers: McpServerEntry[];
}

/** Value type of an editable config field (drives the editor widget). */
export type ConfigFieldKind = 'int' | 'bool' | 'string' | 'list';

/** Metadata about one editable config field. */
export interface ConfigFieldSpec {
  key: string;
  kind: ConfigFieldKind;
  description: string;
}

export interface CommandOutcome {
  handled: boolean;
  record_history: boolean;
  ui: UiRequest | null;
}

/** One skill in the snapshot's skill catalog. */
export interface SkillCatalogEntry {
  name: string;
  description: string;
  disabled: boolean;
}

/** One configured MCP server in the snapshot. */
export interface McpServerInfo {
  name: string;
  enabled: boolean;
  connected: boolean;
  tools: string[];
}

/** Configuration form of one `[[mcp.servers]]` entry (editable on the MCP page). */
export interface McpServerEntry {
  name: string;
  transport: 'stdio' | 'streamable-http';
  /** stdio transport: the executable (resolved via PATH if relative). */
  command: string;
  /** stdio transport: command arguments. */
  args: string[];
  /** stdio transport: extra environment variables for the child process. */
  env: Record<string, string>;
  /** streamable-http transport: the server endpoint URL. */
  url: string;
  /** streamable-http transport: extra HTTP headers (e.g. Authorization). */
  headers: Record<string, string>;
}

/** One agent definition in the snapshot's agent catalog. */
export interface AgentCatalogEntry {
  name: string;
  description: string;
  role: string | null;
  disabled: boolean;
  editable: boolean;
}

/** A saved history session (workspace cwd + first-prompt summary). */
export interface SessionSummary {
  id: number;
  name: string;
  updated_at: number;
  model_id: string;
  cwd: string;
  summary: string;
}

/** Full raw `SKILL.md` contents for the skill preview. */
export interface SkillPreview {
  name: string;
  description: string;
  content: string;
}

/** Detail of one agent definition (raw `.md` content) for the editor. */
export interface AgentDetail {
  name: string;
  description: string;
  role: string | null;
  disabled: boolean;
  editable: boolean;
  source_path: string;
  content: string;
}

/**
 * Internally-tagged serde enum: `Handled(CommandOutcome)` merges the struct
 * fields into the same JSON map next to the `kind` tag.
 */
export type InputLineOutcome =
  | ({ kind: 'handled' } & CommandOutcome)
  | { kind: 'submitted' }
  | { kind: 'empty' };

export interface AppSnapshot {
  status: AppStatus;
  status_message: string;
  session_id: string;
  session_cwd: string;
  current_model: ModelInfo;
  models: ModelInfo[];
  messages: Message[];
  usage: Usage;
  request_count: number;
  active_skills: string[];
  todos: { items: TodoItem[]; next_id: number };
  subagents: SubagentSummary[];
  should_quit: boolean;
  memory_available: boolean;
  memory_session_enabled: boolean;
  skill_catalog: SkillCatalogEntry[];
  mcp_servers: McpServerInfo[];
  agent_catalog: AgentCatalogEntry[];
  config_fields: [string, string][];
  config_field_specs: ConfigFieldSpec[];
  config_scopes: ConfigScopeSnapshot[];
}
