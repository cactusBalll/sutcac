// Tauri bridge implementation of the shared `CatusTransport` interface.
//
// Commands go through Tauri `invoke`, events through Tauri `listen` on the
// channels emitted by `src-tauri/src/actor.rs`.

import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import type {
  AgentDetail,
  AppSnapshot,
  InputLineOutcome,
  McpServerEntry,
  Message,
  ModelEntry,
  RuntimeEventPayload,
  SessionSummary,
  SkillPreview,
} from 'catus-ui';
import type { CatusTransport, TransportHandlers } from 'catus-ui';

export const tauriTransport: CatusTransport = {
  sendInput: (text) => invoke<InputLineOutcome>('send_input', { text }),
  completeInteraction: (answers) => invoke<boolean>('complete_interaction', { answers }),
  cancelInteraction: () => invoke<boolean>('cancel_interaction'),
  overlayAction: (action, value) =>
    invoke<string>('overlay_action', { action, value }),
  getSnapshot: () => invoke<AppSnapshot>('get_snapshot'),
  getSubagentMessages: (id) => invoke<Message[]>('get_subagent_messages', { id }),
  quitApp: () => invoke<void>('quit_app'),
  completionCandidates: (input) =>
    invoke<string[]>('completion_candidates', { input }),
  setConfigField: (scope, key, value) =>
    invoke<string>('set_config_field', { scope, key, value }),
  removeConfigField: (scope, key) =>
    invoke<string>('remove_config_field', { scope, key }),
  upsertModel: (scope, model: ModelEntry) =>
    invoke<string>('upsert_model', { scope, model }),
  removeModel: (scope, id) =>
    invoke<string>('remove_model', { scope, id }),
  upsertMcpServer: (scope, server: McpServerEntry) =>
    invoke<string>('upsert_mcp_server', { scope, server }),
  removeMcpServer: (scope, name) =>
    invoke<string>('remove_mcp_server', { scope, name }),
  listSessions: (scope) => invoke<SessionSummary[]>('list_sessions', { scope }),
  skillPreview: (name) => invoke<SkillPreview>('skill_preview', { name }),
  agentDetail: (name) => invoke<AgentDetail>('get_agent_detail', { name }),
  saveAgent: (name, content) => invoke<string>('save_agent', { name, content }),
  createAgent: (name, content) => invoke<string>('create_agent', { name, content }),
  async subscribe(handlers: TransportHandlers) {
    // `app-quit` closes the window on the Rust side; nothing to render.
    // The in-process event channel stays up for the app's lifetime, so the
    // connection is reported once subscription succeeds (or fails).
    try {
      await Promise.all([
        listen<RuntimeEventPayload>('runtime-event', (e) => handlers.onRuntimeEvent(e.payload)),
        listen<AppSnapshot>('snapshot', (e) => handlers.onSnapshot(e.payload)),
        listen<string>('startup-error', (e) => handlers.onStartupError(String(e.payload))),
        listen('app-quit', () => handlers.onQuit()),
      ]);
      handlers.onConnectionChange?.(true);
    } catch (e) {
      handlers.onConnectionChange?.(false);
      throw e;
    }
  },
};
