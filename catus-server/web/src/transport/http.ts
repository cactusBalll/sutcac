// HTTP/WebSocket implementation of the shared `CatusTransport` interface.
//
// Commands go through the catus-server REST routes (one per Tauri command),
// events through a single WebSocket connection that auto-reconnects. The
// server sends the current snapshot on every (re)connect, so no extra
// resync logic is needed.

import type {
  AgentDetail,
  AppSnapshot,
  AskAnswer,
  ConfigScope,
  InputLineOutcome,
  Message,
  RuntimeEventPayload,
  SessionSummary,
  SkillPreview,
} from 'catus-ui';
import type { CatusTransport, TransportHandlers } from 'catus-ui';

const RECONNECT_DELAY_MS = 1000;

async function post<T>(path: string, body: unknown): Promise<T> {
  const res = await fetch(path, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body ?? {}),
  });
  return read_body<T>(res);
}

async function get<T>(path: string): Promise<T> {
  return read_body<T>(await fetch(path));
}

async function read_body<T>(res: Response): Promise<T> {
  if (!res.ok) {
    const text = (await res.text().catch(() => '')) || `HTTP ${res.status}`;
    throw new Error(text);
  }
  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

interface EventFrame {
  channel: 'runtime-event' | 'snapshot' | 'startup-error' | 'app-quit';
  payload: unknown;
}

export const httpTransport: CatusTransport = {
  sendInput: (text) => post<InputLineOutcome>('/api/input', { text }),
  completeInteraction: (answers: AskAnswer[]) =>
    post<boolean>('/api/interaction/complete', { answers }),
  cancelInteraction: () => post<boolean>('/api/interaction/cancel', {}),
  overlayAction: (action: string, value: string) =>
    post<string>('/api/overlay', { action, value }),
  getSnapshot: () => get<AppSnapshot>('/api/snapshot'),
  getSubagentMessages: (id: string) =>
    get<Message[]>(`/api/subagents/${encodeURIComponent(id)}/messages`),
  quitApp: () => post<void>('/api/quit', {}),
  completionCandidates: (input: string) =>
    get<string[]>(`/api/completion?input=${encodeURIComponent(input)}`),
  setConfigField: (scope: ConfigScope, key: string, value: string) =>
    post<string>('/api/config', { scope, key, value }),
  removeConfigField: (scope: ConfigScope, key: string) =>
    post<string>('/api/config/remove', { scope, key }),
  listSessions: (scope: string) =>
    get<SessionSummary[]>(`/api/sessions?scope=${encodeURIComponent(scope)}`),
  skillPreview: (name: string) =>
    get<SkillPreview>(`/api/skills/${encodeURIComponent(name)}/preview`),
  agentDetail: (name: string) =>
    get<AgentDetail>(`/api/agents/${encodeURIComponent(name)}`),
  saveAgent: (name: string, content: string) =>
    post<string>(`/api/agents/${encodeURIComponent(name)}`, { content }),
  createAgent: (name: string, content: string) =>
    post<string>('/api/agents', { name, content }),
  subscribe(handlers: TransportHandlers) {
    return new Promise<void>((resolve) => {
      connect(handlers, resolve);
    });
  },
};

function ws_url(): string {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${proto}//${location.host}/ws`;
}

/** Connect (and reconnect after any close until `app-quit`). */
function connect(handlers: TransportHandlers, ready: () => void, attempt = 0): void {
  const socket = new WebSocket(ws_url());
  let open = false;
  let closed = false;

  socket.onopen = () => {
    open = true;
    ready();
  };
  socket.onmessage = (message) => {
    let frame: EventFrame;
    try {
      frame = JSON.parse(String(message.data)) as EventFrame;
    } catch {
      return;
    }
    switch (frame.channel) {
      case 'runtime-event':
        handlers.onRuntimeEvent(frame.payload as RuntimeEventPayload);
        break;
      case 'snapshot':
        handlers.onSnapshot(frame.payload as AppSnapshot);
        break;
      case 'startup-error':
        handlers.onStartupError(String(frame.payload));
        break;
      case 'app-quit':
        handlers.onQuit();
        closed = true; // The session is over; stop reconnecting.
        return;
    }
  };
  socket.onclose = () => {
    if (closed) return;
    if (!open) {
      // The server may still be booting (or the UI is served before the
      // process started); retry with a capped backoff.
      attempt = Math.min(attempt + 1, 5);
      setTimeout(() => connect(handlers, ready, attempt), attempt * RECONNECT_DELAY_MS);
      return;
    }
    // Live connections come back after server restarts and network blips;
    // the snapshot pushed on the next connect resyncs the state.
    setTimeout(() => connect(handlers, ready), RECONNECT_DELAY_MS);
  };
}
