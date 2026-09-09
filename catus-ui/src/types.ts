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
  config_fields: [string, string][];
  config_field_specs: ConfigFieldSpec[];
  config_scopes: ConfigScopeSnapshot[];
}
