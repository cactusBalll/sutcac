//! Frontend-independent runtime orchestration.
//!
//! The [`Runtime`] wraps an [`App`] and merges all asynchronous event
//! sources — the LLM stream, stream completion, and subagent events — into a
//! single [`RuntimeEvent`] stream that any frontend can consume: the TUI
//! event loop today, a Web/WebSocket or Agent Client Protocol adapter later.
//!
//! Frontends interact with the runtime through semantic actions
//! (`App::handle_input_line`, `complete_interaction`, commands) and drive
//! the turn state machine exclusively via [`Runtime::next_event`]; they
//! never own channel plumbing or orchestration logic.

use crate::app::{App, RuntimeEvent, TurnPhase};
use crate::llm::StreamEvent;

/// Merged event source over the application's turn state machine.
pub struct Runtime {
    /// The frontend-independent application state.
    pub app: App,
}

/// Map a raw stream event to the event surfaced to the frontend.
fn map_stream_event(event: &StreamEvent) -> RuntimeEvent {
    match event {
        StreamEvent::Text(text) => RuntimeEvent::StreamText(text.clone()),
        StreamEvent::Reasoning(text) => RuntimeEvent::StreamReasoning(text.clone()),
        StreamEvent::ToolCall(call) => RuntimeEvent::ToolCallAdded(call.clone()),
        StreamEvent::Usage(usage) => RuntimeEvent::UsageUpdated(*usage),
    }
}

impl Runtime {
    /// Wrap an application in a runtime.
    pub fn new(app: App) -> Self {
        Self { app }
    }

    /// Resume the LLM stream unless the memory recall pass is still running
    /// (the stream then starts when the pass completes).
    pub async fn maybe_resume_stream(&mut self) {
        if !self.app.awaiting_memory_recall() {
            self.app.start_llm_stream().await;
        }
    }

    /// Wait for the next runtime event, driving the turn state machine.
    ///
    /// Internally selects over the LLM stream channel, the stream-completion
    /// channel, and the subagent event channel; completion of a stream also
    /// drains any queued stream events first so tool calls and text chunks
    /// are never applied after the turn phase advances.
    ///
    /// Returns `None` only when every event source has been closed.
    pub async fn next_event(&mut self) -> Option<RuntimeEvent> {
        loop {
            // Events queued by synchronous handlers (interaction requests,
            // message-change notifications) always surface first.
            if let Some(event) = self.app.take_event() {
                return Some(event);
            }

            tokio::select! {
                Some(event) = self.app.stream_rx.recv() => {
                    let mapped = map_stream_event(&event);
                    self.app.handle_stream_event(event);
                    return Some(mapped);
                }
                Some(result) = self.app.done_rx.recv() => {
                    // The stream task sends every StreamEvent *before*
                    // signalling done, but tokio::select! may pick the done
                    // branch first even when queued events are still waiting.
                    // Drain them so tool calls and text chunks are never
                    // applied after completion.
                    while let Ok(event) = self.app.stream_rx.try_recv() {
                        let mapped = map_stream_event(&event);
                        self.app.handle_stream_event(event);
                        self.app.queue_event(mapped);
                    }
                    match self.app.handle_llm_done(result).await {
                        TurnPhase::ContinueStream => {
                            self.app.start_llm_stream().await;
                        }
                        TurnPhase::PausedInteraction => {
                            // The InteractionRequested event queued by
                            // `run_pending_tool` surfaces next; the turn
                            // resumes when the frontend answers.
                        }
                        TurnPhase::Complete => {
                            self.app.queue_event(RuntimeEvent::TurnComplete);
                        }
                    }
                }
                Some(event) = self.app.subagents.event_rx.recv() => {
                    let should_resume = self.app.handle_subagent_event(event.clone());
                    if should_resume {
                        self.app.start_llm_stream().await;
                    }
                    return Some(RuntimeEvent::SubagentEvent(event));
                }
                else => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::config::AppConfig;

    /// Config whose only provider points at a dead local port so LLM
    /// requests fail fast without network access.
    fn dead_endpoint_config() -> AppConfig {
        let mut config = AppConfig::default();
        config.providers.push(crate::llm::Provider {
            name: "test".to_string(),
            base_url: "http://127.0.0.1:9/v1".to_string(),
            api_key: "test".to_string(),
            session_header: None,
        });
        config.models.push(crate::config::ModelEntry {
            id: "test-model".to_string(),
            name: "Test Model".to_string(),
            context_window: 4096,
            provider: "test".to_string(),
        });
        config.agent.auto_include_skills = false;
        config
    }

    #[tokio::test]
    async fn next_event_surfaces_queued_events_in_order() {
        let mut runtime = Runtime::new(App::new(AppConfig::default()));

        runtime.app.submit_user_message("hello".to_string());

        // The submission queues a message-change notification.
        let first = runtime.next_event().await;
        assert!(matches!(first, Some(RuntimeEvent::MessagesChanged)));
    }

    #[tokio::test]
    async fn next_event_drives_a_failing_turn_to_completion() {
        let mut runtime = Runtime::new(App::new(dead_endpoint_config()));

        runtime.app.submit_user_message("hello".to_string());
        // Skip the submission notification.
        let _ = runtime.next_event().await;

        runtime.maybe_resume_stream().await;

        // The stream fails against the dead endpoint; the runtime then
        // surfaces the failure notice and completes the turn.
        let mut saw_failure_notice = false;
        let mut saw_turn_complete = false;
        for _ in 0..16 {
            match runtime.next_event().await {
                Some(RuntimeEvent::MessagesChanged) => {
                    if runtime
                        .app
                        .messages
                        .iter()
                        .any(|m| m.is_event() && m.content.contains("LLM request failed"))
                    {
                        saw_failure_notice = true;
                    }
                }
                Some(RuntimeEvent::TurnComplete) => {
                    saw_turn_complete = true;
                    break;
                }
                Some(_) => {}
                None => break,
            }
        }
        assert!(saw_failure_notice, "the failure must be surfaced");
        assert!(saw_turn_complete, "the turn must complete after the error");
        assert!(runtime.app.status == crate::app::AppStatus::Error);
    }

    #[tokio::test]
    async fn cancelled_interaction_resumes_the_turn() {
        let mut runtime = Runtime::new(App::new(dead_endpoint_config()));

        // Simulate a paused interaction exactly as `run_pending_tool` would
        // leave it: a pending ask_user call plus the interaction event.
        runtime
            .app
            .messages
            .push(crate::message::Message::assistant(String::new()));
        runtime.app.add_tool_call(crate::tool::ToolCall {
            id: "call_ask".to_string(),
            name: "ask_user".to_string(),
            arguments: r#"{"questions":[{"prompt":"Pick","title":"Pick","options":[{"label":"a"},{"label":"b"}],"multiSelect":false}]}"#.to_string(),
        });
        runtime.app.run_pending_tool().await;
        assert!(runtime.app.has_pending_interaction());

        // The frontend answers by cancelling; the tool result is appended and
        // the turn resumes without a live model round-trip (the ask result
        // alone yields no new tool calls).
        assert!(runtime.app.cancel_interaction());
        runtime.maybe_resume_stream().await;

        let mut saw_turn_complete = false;
        for _ in 0..16 {
            match runtime.next_event().await {
                Some(RuntimeEvent::TurnComplete) => {
                    saw_turn_complete = true;
                    break;
                }
                Some(_) => {}
                None => break,
            }
        }
        assert!(saw_turn_complete);
        assert!(!runtime.app.has_pending_interaction());
        assert!(
            runtime
                .app
                .messages
                .iter()
                .any(|m| m.content.contains("user cancelled"))
        );
    }
}
