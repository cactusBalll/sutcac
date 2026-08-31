//! Modal overlay state for the TUI.

/// Modal page shown on top of the chat view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    /// No overlay; keys go to the input line.
    None,
    /// Interactive history picker. `items` are history file stems (newest
    /// first), `selected` is the highlighted entry.
    Resume { items: Vec<String>, selected: usize },
    /// Token usage details.
    Status,
    /// Config editor.
    Config { selected: usize },
    /// Skill picker.
    Skills { items: Vec<String>, selected: usize },
}

impl Overlay {
    pub fn is_active(&self) -> bool {
        !matches!(self, Overlay::None)
    }
}

impl Default for Overlay {
    fn default() -> Self {
        Overlay::None
    }
}

/// Mutable state for the currently active modal overlay.
#[derive(Debug, Clone, Default)]
pub struct OverlayState {
    /// Modal overlay currently displayed on top of the chat view.
    pub overlay: Overlay,
}

impl OverlayState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a modal overlay is currently displayed.
    pub fn is_active(&self) -> bool {
        self.overlay.is_active()
    }

    /// Replace the active overlay with the resume picker.
    pub fn open_resume(&mut self, items: Vec<String>) {
        self.overlay = Overlay::Resume { items, selected: 0 };
    }

    /// Replace the active overlay with the status page.
    pub fn open_status(&mut self) {
        self.overlay = Overlay::Status;
    }

    /// Replace the active overlay with the config editor.
    pub fn open_config(&mut self) {
        self.overlay = Overlay::Config { selected: 0 };
    }

    /// Replace the active overlay with the skill picker.
    pub fn open_skills(&mut self, items: Vec<String>) {
        self.overlay = Overlay::Skills { items, selected: 0 };
    }

    /// Close any active overlay.
    pub fn close(&mut self) {
        self.overlay = Overlay::None;
    }
}
