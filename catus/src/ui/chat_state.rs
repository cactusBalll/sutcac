//! Chat view state: scroll position and auto-scroll behavior.

/// Mutable state for the chat history viewport.
#[derive(Debug, Clone, Default)]
pub struct ChatState {
    /// Scroll offset from the bottom of the history (0 = latest message).
    pub scroll: usize,
    /// Whether new content should automatically scroll to the bottom.
    pub auto_scroll: bool,
}

impl ChatState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Scroll up by `amount` lines, disabling auto-scroll.
    pub fn scroll_up(&mut self, amount: usize) {
        self.scroll = self.scroll.saturating_add(amount);
        self.auto_scroll = false;
    }

    /// Scroll down by `amount` lines, re-enabling auto-scroll when reaching
    /// the bottom.
    pub fn scroll_down(&mut self, amount: usize) {
        self.scroll = self.scroll.saturating_sub(amount);
        if self.scroll == 0 {
            self.auto_scroll = true;
        }
    }

    /// Jump to the bottom of the history and enable auto-scroll.
    pub fn scroll_to_bottom(&mut self) {
        self.scroll = 0;
        self.auto_scroll = true;
    }

    /// Jump to the top of the history and disable auto-scroll.
    pub fn scroll_to_top(&mut self) {
        self.scroll = usize::MAX;
        self.auto_scroll = false;
    }
}
