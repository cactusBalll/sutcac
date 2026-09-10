import { defineStore } from 'pinia';
import type { CatusTransport } from '../transport';
import type {
  AppSnapshot,
  AskAnswer,
  AskQuestion,
  ConfigScope,
  Message,
  RuntimeEventPayload,
  SessionSummary,
  UiRequest,
} from '../types';

/** Backend connection, injected by `init` (Tauri bridge or HTTP/WS). */
let transport: CatusTransport | null = null;

function backend(): CatusTransport {
  if (!transport) throw new Error('runtime store not initialized');
  return transport;
}

export type Overlay =
  | { kind: 'ask'; questions: AskQuestion[] }
  | { kind: 'ui'; request: UiRequest }
  | null;

/** Full-screen manager pages reachable from the left sidebar. */
export type SidePanel = 'sessions' | 'skills' | 'mcp' | 'agents' | null;

export const useRuntimeStore = defineStore('runtime', {
  state: () => ({
    started: false,
    startupError: '',
    /** True once the backend announced shutdown (web: show an end banner). */
    quitRequested: false,
    messages: [] as Message[],
    /** Live typing buffers filled by stream deltas between snapshots. */
    streamingText: '',
    streamingReasoning: '',
    /** True while the last assistant message may still grow. */
    streaming: false,
    status: 'idle' as AppSnapshot['status'],
    statusMessage: '',
    usage: { prompt_tokens: 0, completion_tokens: 0, total_tokens: 0, cached_tokens: 0 },
    requestCount: 0,
    currentModel: { id: '', name: '', context_window: 0 },
    models: [] as AppSnapshot['models'],
    subagents: [] as AppSnapshot['subagents'],
    activeSkills: [] as string[],
    todos: { items: [], next_id: 0 } as AppSnapshot['todos'],
    configFields: [] as [string, string][],
    configFieldSpecs: [] as AppSnapshot['config_field_specs'],
    configScopes: [] as AppSnapshot['config_scopes'],
    sessionCwd: '',
    memoryAvailable: false,
    memorySessionEnabled: false,
    skillCatalog: [] as AppSnapshot['skill_catalog'],
    mcpServers: [] as AppSnapshot['mcp_servers'],
    agentCatalog: [] as AppSnapshot['agent_catalog'],
    /** Sessions of the current workspace (sidebar, newest 10). */
    recentSessions: [] as SessionSummary[],
    /** Every session of every workspace (full-screen history page). */
    allSessions: [] as SessionSummary[],
    /** Currently open full-screen sidebar page, if any. */
    panel: null as SidePanel,
    /** Whether the left sidebar is expanded (vs. an icon rail). */
    sidebarOpen: true,
    /** Auto-scroll follows the stream unless the user scrolled up. */
    autoScroll: true,
    overlay: null as Overlay,
    /** Pending tool calls shown while waiting for their result message. */
    pendingToolCalls: [] as { id: string; name: string }[],
    /** Subagent currently displayed on the monitor page. */
    watching: null as string | null,
    watchingMessages: [] as Message[],
    inputHistory: [] as string[],
    /** Shared input line so overlays (e.g. agent picker) can prefill it. */
    inputText: '',
    /** Slash-command/history completion candidates for the input line. */
    candidates: [] as string[],
    /** Highlighted candidate index; `null` until the user cycles. */
    selectedCandidate: null as number | null,
    /** Guard against out-of-order completion responses. */
    completionSeq: 0,
  }),

  getters: {
    busy: (state) => state.status === 'streaming' || state.status === 'running_tool',
    /** The visible conversation: system messages stay hidden. */
    visibleMessages: (state) => state.messages.filter((m) => m.role !== 'system'),
  },

  actions: {
    async init(newTransport: CatusTransport) {
      transport = newTransport;
      await transport.subscribe({
        onRuntimeEvent: (event) => this.onRuntimeEvent(event),
        onSnapshot: (snapshot) => this.applySnapshot(snapshot),
        onStartupError: (message) => {
          this.startupError = message;
        },
        onQuit: () => {
          this.quitRequested = true;
        },
      });
      try {
        this.applySnapshot(await transport.getSnapshot());
      } catch {
        // The actor may not be ready yet; events will refresh the state.
      }
      this.started = true;
    },

    applySnapshot(snap: AppSnapshot) {
      const hadStreamed = this.streamingText.length > 0 || this.streamingReasoning.length > 0;
      this.messages = snap.messages;
      this.status = snap.status;
      this.statusMessage = snap.status_message;
      this.usage = snap.usage;
      this.requestCount = snap.request_count;
      this.currentModel = snap.current_model;
      this.models = snap.models;
      this.subagents = snap.subagents;
      this.activeSkills = snap.active_skills;
      this.todos = snap.todos;
      this.configFields = snap.config_fields;
      this.configFieldSpecs = snap.config_field_specs;
      this.configScopes = snap.config_scopes;
      this.sessionCwd = snap.session_cwd;
      this.memoryAvailable = snap.memory_available;
      this.memorySessionEnabled = snap.memory_session_enabled;
      this.skillCatalog = snap.skill_catalog ?? [];
      this.mcpServers = snap.mcp_servers ?? [];
      this.agentCatalog = snap.agent_catalog ?? [];

      // Streamed deltas are always already included in the core messages by
      // the time a snapshot is emitted, so the live buffer can be dropped
      // once its text is contained in the tail of the conversation.
      if (hadStreamed) {
        const last = snap.messages[snap.messages.length - 1];
        const tail = last?.role === 'assistant' ? last : undefined;
        if (
          tail &&
          (this.streamingText === '' || tail.content.includes(this.streamingText)) &&
          (this.streamingReasoning === '' || (tail.reasoning_content ?? '').includes(this.streamingReasoning))
        ) {
          this.streamingText = '';
          this.streamingReasoning = '';
          this.pendingToolCalls = [];
        }
      }
      if (snap.status === 'idle' || snap.status === 'error') {
        this.streaming = false;
        if (snap.status === 'idle') {
          this.streamingText = '';
          this.streamingReasoning = '';
          this.pendingToolCalls = [];
        }
      }
      if (snap.should_quit) {
        backend().quitApp().catch(() => {});
      }
    },

    onRuntimeEvent(event: RuntimeEventPayload) {
      switch (event.type) {
        case 'stream_text':
          this.streaming = true;
          this.streamingText += event.text;
          break;
        case 'stream_reasoning':
          this.streaming = true;
          this.streamingReasoning += event.text;
          break;
        case 'tool_call_added':
          this.pendingToolCalls.push({ id: event.call.id, name: event.call.function.name });
          break;
        case 'usage_updated':
          this.usage = event.usage;
          break;
        case 'messages_changed':
          this.refreshSnapshot();
          break;
        case 'interaction_requested':
          this.overlay = { kind: 'ask', questions: event.questions };
          break;
        case 'subagent_event':
          if (this.watching === event.event.id) {
            this.refreshWatch();
          }
          break;
        case 'turn_complete':
          this.streaming = false;
          this.refreshSnapshot();
          break;
      }
    },

    async refreshSnapshot() {
      try {
        this.applySnapshot(await backend().getSnapshot());
      } catch (e) {
        console.error('get_snapshot failed', e);
      }
    },

    async sendInput(text: string) {
      const trimmed = text.trim();
      if (!trimmed) return;
      try {
        const outcome = await backend().sendInput(trimmed);
        if (outcome.kind === 'empty') return;
        this.inputHistory.push(trimmed);
        if (outcome.kind === 'submitted') {
          this.streaming = true;
          // The user message arrives through the next snapshot.
          this.refreshSnapshot();
          return;
        }
        // Handled: `ui` requests are already reflected via the snapshot
        // emitted by the actor; realize the presentation intent here.
        if (outcome.ui) {
          this.applyRequest(outcome.ui);
        }
      } catch (e) {
        this.status = 'error';
        this.statusMessage = String(e);
      }
    },

    applyRequest(request: UiRequest) {
      if (request.page === 'watch_subagent') {
        this.watching = request.id;
        this.overlay = { kind: 'ui', request };
        this.refreshWatch();
        return;
      }
      this.overlay = { kind: 'ui', request };
    },

    /** Open the config editor page (the bare-`/config` menu). */
    openConfigPage() {
      this.applyRequest({ page: 'show_config' });
    },

    /** Open a full-screen sidebar page. */
    async openPanel(panel: Exclude<SidePanel, null>) {
      this.panel = panel;
      if (panel === 'sessions') await this.refreshSessions();
    },

    closePanel() {
      this.panel = null;
    },

    /** Refresh the sidebar history list and the full history page. */
    async refreshSessions() {
      try {
        const [recent, all] = await Promise.all([
          backend().listSessions('current'),
          backend().listSessions('all'),
        ]);
        this.recentSessions = recent;
        this.allSessions = all;
      } catch (e) {
        console.error('list_sessions failed', e);
      }
    },

    /** Start a new context: save the session, reset the conversation.
     *  An optional path fully switches the workspace. */
    async newContext(path = '') {
      await this.pick('new_session', path);
      await this.refreshSessions();
    },

    /** Toggle a skill for the LLM (the backend picks the direction). */
    async toggleSkill(name: string) {
      await this.pick('toggle_skill', name);
    },

    /** Toggle an MCP server (the backend picks the direction). */
    async toggleMcp(name: string) {
      await this.pick('toggle_mcp', name);
    },

    /** Toggle an agent's dispatch availability. */
    async toggleAgent(name: string) {
      await this.pick('toggle_agent', name);
    },

    /** Connect to one MCP server on demand. */
    async connectMcp(name: string) {
      await this.pick('connect_mcp', name);
    },

    /** Full raw `SKILL.md` contents for the skill preview. */
    async skillPreview(name: string) {
      return backend().skillPreview(name);
    },

    /** Detail of one agent definition (raw `.md` content). */
    async agentDetail(name: string) {
      return backend().agentDetail(name);
    },

    /** Save an edited agent definition. Refreshes the catalog on success. */
    async saveAgent(name: string, content: string): Promise<string> {
      const msg = await backend().saveAgent(name, content);
      await this.refreshSnapshot();
      return msg;
    },

    /** Create a new agent definition. Refreshes the catalog on success. */
    async createAgent(name: string, content: string): Promise<string> {
      const msg = await backend().createAgent(name, content);
      await this.refreshSnapshot();
      return msg;
    },

    /** Open the model picker from the status bar (the bare-`/model` menu). */
    openModelPicker() {      if (!this.models.length) return;
      const selected = Math.max(
        0,
        this.models.findIndex((m) => m.id === this.currentModel.id),
      );
      this.applyRequest({
        page: 'show_models',
        items: this.models.map((m) => m.id),
        selected,
      });
    },

    /** A picker choice or monitor action from an overlay. */
    async pick(action: string, value: string) {
      try {
        await backend().overlayAction(action, value);
      } catch (e) {
        this.status = 'error';
        this.statusMessage = String(e);
      }
    },

    /** Set a config field in one scope. Returns success for inline UI. */
    async setConfigField(scope: ConfigScope, key: string, value: string): Promise<boolean> {
      try {
        await backend().setConfigField(scope, key, value);
      } catch (e) {
        this.status = 'error';
        this.statusMessage = String(e);
        return false;
      }
      await this.refreshSnapshot();
      return true;
    },

    /** Remove a config field from one scope. Returns success for inline UI. */
    async removeConfigField(scope: ConfigScope, key: string): Promise<boolean> {
      try {
        await backend().removeConfigField(scope, key);
      } catch (e) {
        this.status = 'error';
        this.statusMessage = String(e);
        return false;
      }
      await this.refreshSnapshot();
      return true;
    },

    closeOverlay() {
      this.overlay = null;
      this.watching = null;
      this.watchingMessages = [];
    },

    async answerInteraction(answers: AskAnswer[]) {
      this.overlay = null;
      try {
        const resumed = await backend().completeInteraction(answers);
        if (resumed) this.streaming = true;
      } catch (e) {
        this.status = 'error';
        this.statusMessage = String(e);
      }
    },

    async cancelInteraction() {
      this.overlay = null;
      try {
        const resumed = await backend().cancelInteraction();
        if (resumed) this.streaming = true;
      } catch (e) {
        this.status = 'error';
        this.statusMessage = String(e);
      }
    },

    async refreshWatch() {
      if (!this.watching) return;
      try {
        this.watchingMessages = await backend().getSubagentMessages(this.watching);
      } catch {
        this.watchingMessages = [];
      }
    },

    /** Recompute completion candidates for the current input line. */
    async recomputeCandidates() {
      const text = this.inputText;
      // After a Tab fill the input equals one of the candidates: keep the
      // list and the highlight so continued cycling walks the same set
      // (the TUI only recomputes on real edits, not on candidate fills).
      if (text && this.candidates.includes(text)) return;
      this.selectedCandidate = null;
      if (!text) {
        this.candidates = [];
        return;
      }
      if (text.startsWith('/')) {
        const seq = ++this.completionSeq;
        let list: string[];
        try {
          list = await backend().completionCandidates(text);
        } catch {
          return;
        }
        // A newer keystroke superseded this response.
        if (seq !== this.completionSeq) return;
        this.candidates = list;
      } else {
        // Plain text: complete from the session history, newest first.
        const prefix = text.toLowerCase();
        const seen = new Set<string>();
        const list: string[] = [];
        for (const entry of [...this.inputHistory].reverse()) {
          if (entry.toLowerCase().startsWith(prefix) && !seen.has(entry)) {
            seen.add(entry);
            list.push(entry);
          }
        }
        this.candidates = list;
      }
    },

    /**
     * Cycle through completion candidates (Tab / Shift+Tab / ↑↓ on slash
     * commands) and fill the input with the selection, mirroring the TUI's
     * `InputState::cycle_candidate`. Wraps around at both ends.
     */
    cycleCandidate(delta: number) {
      if (!this.candidates.length) return;
      const len = this.candidates.length;
      let idx: number;
      if (this.selectedCandidate === null) {
        idx = delta >= 0 ? 0 : len - 1;
      } else {
        idx = (((this.selectedCandidate + delta) % len) + len) % len;
      }
      this.selectedCandidate = idx;
      this.inputText = this.candidates[idx];
    },

    /** Pick a candidate directly (mouse click). */
    chooseCandidate(index: number) {
      if (index < 0 || index >= this.candidates.length) return;
      this.selectedCandidate = index;
      this.inputText = this.candidates[index];
    },

    /** Drop the highlight without changing the input (Esc). */
    clearCandidateSelection() {
      this.selectedCandidate = null;
    },
  },
});
