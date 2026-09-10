// Transport abstraction between the shared UI and a catus backend.
//
// Two implementations exist:
// - catus-web/src/transport/tauri.ts: Tauri invoke/listen bridge
// - catus-server/web/src/transport/http.ts: REST commands + WebSocket events
//
// Payload types mirror the serde shapes of the catus-core runtime boundary.

import type {
  AgentDetail,
  AppSnapshot,
  AskAnswer,
  ConfigScope,
  InputLineOutcome,
  McpServerEntry,
  Message,
  ModelEntry,
  RuntimeEventPayload,
  SessionSummary,
  SkillPreview,
} from './types';

export interface TransportHandlers {
  /** A serialized RuntimeEvent pushed by the runtime actor. */
  onRuntimeEvent(event: RuntimeEventPayload): void;
  /** A full AppSnapshot pushed after structural state changes. */
  onSnapshot(snapshot: AppSnapshot): void;
  /** Runtime bootstrap failed; the session cannot be served. */
  onStartupError(message: string): void;
  /** The session requested a shutdown (`/exit`). */
  onQuit(): void;
  /** Transport connectivity changed (e.g. the WS dropped and is reconnecting). */
  onConnectionChange?(connected: boolean): void;
}

export interface CatusTransport {
  sendInput(text: string): Promise<InputLineOutcome>;
  completeInteraction(answers: AskAnswer[]): Promise<boolean>;
  cancelInteraction(): Promise<boolean>;
  overlayAction(action: string, value: string): Promise<string>;
  getSnapshot(): Promise<AppSnapshot>;
  getSubagentMessages(id: string): Promise<Message[]>;
  quitApp(): Promise<void>;
  /** Slash-command completion candidates for the current input prefix. */
  completionCandidates(input: string): Promise<string[]>;
  /** Set a config field in one scope; resolves with the status message. */
  setConfigField(scope: ConfigScope, key: string, value: string): Promise<string>;
  /** Remove a config field from one scope; resolves with the status message. */
  removeConfigField(scope: ConfigScope, key: string): Promise<string>;
  /** Insert or update one `[[models]]` entry in one scope; resolves with the status message. */
  upsertModel(scope: ConfigScope, model: ModelEntry): Promise<string>;
  /** Remove one `[[models]]` entry (by id) from one scope; resolves with the status message. */
  removeModel(scope: ConfigScope, id: string): Promise<string>;
  /** Insert or update one `[[mcp.servers]]` entry in one scope; resolves with the status message. */
  upsertMcpServer(scope: ConfigScope, server: McpServerEntry): Promise<string>;
  /** Remove one `[[mcp.servers]]` entry (by name) from one scope; resolves with the status message. */
  removeMcpServer(scope: ConfigScope, name: string): Promise<string>;
  /**
   * List history sessions. `scope='all'` returns every workspace's
   * sessions; the default returns the current workspace's 10 newest.
   */
  listSessions(scope: string): Promise<SessionSummary[]>;
  /** Full raw `SKILL.md` contents for the skill preview. */
  skillPreview(name: string): Promise<SkillPreview>;
  /** Detail of one agent definition (raw `.md` content) for the editor. */
  agentDetail(name: string): Promise<AgentDetail>;
  /** Save an edited agent definition back to its source file. */
  saveAgent(name: string, content: string): Promise<string>;
  /** Create a new agent definition in the workspace agents directory. */
  createAgent(name: string, content: string): Promise<string>;
  /** Subscribe to pushed events. Resolves once subscription is active. */
  subscribe(handlers: TransportHandlers): Promise<void>;
}
