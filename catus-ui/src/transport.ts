// Transport abstraction between the shared UI and a catus backend.
//
// Two implementations exist:
// - catus-web/src/transport/tauri.ts: Tauri invoke/listen bridge
// - catus-server/web/src/transport/http.ts: REST commands + WebSocket events
//
// Payload types mirror the serde shapes of the catus-core runtime boundary.

import type {
  AppSnapshot,
  AskAnswer,
  ConfigScope,
  InputLineOutcome,
  Message,
  RuntimeEventPayload,
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
  /** Subscribe to pushed events. Resolves once subscription is active. */
  subscribe(handlers: TransportHandlers): Promise<void>;
}
