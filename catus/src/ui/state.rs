//! TUI-owned view state.
//!
//! The core runtime ([`catus_core::app::App`]) holds only session state that
//! every frontend needs. Anything that exists purely to drive one frontend's
//! presentation — the input line, chat scroll position, and modal overlays —
//! lives here in [`UiState`], owned by the TUI layer. Other frontends are
//! free to model interaction however they like.

use crate::ui::chat_state::ChatState;
use crate::ui::input_state::InputState;
use crate::ui::overlay_state::OverlayState;
use catus_core::app::UiRequest;

/// Mutable TUI-side state: input line, chat viewport, and modal overlays.
#[derive(Debug, Clone, Default)]
pub struct UiState {
    /// Input-line state (cursor, history, completion candidates).
    pub input_state: InputState,
    /// Chat viewport state (scroll, auto-scroll).
    pub chat_state: ChatState,
    /// Modal overlay state.
    pub overlay_state: OverlayState,
}

impl UiState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Realize a core presentation intent by opening the matching modal
    /// overlay. This is the only place where core `UiRequest`s map onto TUI
    /// pages.
    pub fn apply_request(&mut self, request: UiRequest) {
        match request {
            UiRequest::ShowStatus => self.overlay_state.open_status(),
            UiRequest::ShowConfig => self.overlay_state.open_config(),
            UiRequest::ShowResumePicker { items } => self.overlay_state.open_resume(items),
            UiRequest::ShowSkills { items } => self.overlay_state.open_skills(items),
            UiRequest::ShowModels { items, selected } => {
                self.overlay_state.open_model(items, selected)
            }
            UiRequest::ShowAgents { items } => self.overlay_state.open_agents(items),
            UiRequest::ShowSubagents { items } => self.overlay_state.open_subagent_status(items),
            UiRequest::WatchSubagent { id } => self.overlay_state.open_subagent_watch(Some(id)),
            UiRequest::CloseSubagentPicker { items } => {
                self.overlay_state.open_subagent_close(items)
            }
        }
    }
}
