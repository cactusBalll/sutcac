//! Input-line state: typed text, cursor, history recall, and completion
//! candidates.

use crate::app::commands::BUILT_IN_REGISTRY;

/// Maximum number of rows reserved for the completion candidate strip.
pub const MAX_CANDIDATES: usize = 8;

/// Mutable state for the one-line input box.
#[derive(Debug, Clone, Default)]
pub struct InputState {
    /// Current content of the input line.
    pub input: String,
    /// Byte index of the cursor inside `input`. Always aligned to a UTF-8
    /// character boundary.
    pub cursor: usize,
    /// Submitted user inputs in the current session, newest first.
    pub input_history: Vec<String>,
    /// Index into `input_history` when recalling a previous input.
    /// `None` means the user is editing a fresh line.
    pub input_history_index: Option<usize>,
    /// The line being typed before history recall started, restored by Down.
    pub draft_input: String,
    /// Current completion candidates shown below the input box.
    pub candidates: Vec<String>,
    /// Currently selected candidate index, if any.
    pub selected_candidate: Option<usize>,
}

impl InputState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a character at the cursor and move right.
    pub fn push_char(&mut self, c: char) {
        self.input_history_index = None;
        self.input.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        self.recompute_candidates();
    }

    /// Delete the character before the cursor.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.input_history_index = None;
        let prev = prev_char_boundary(&self.input, self.cursor);
        self.input.replace_range(prev..self.cursor, "");
        self.cursor = prev;
        self.recompute_candidates();
    }

    /// Move the cursor one character left.
    pub fn move_cursor_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor = prev_char_boundary(&self.input, self.cursor);
    }

    /// Move the cursor one character right.
    pub fn move_cursor_right(&mut self) {
        if self.cursor >= self.input.len() {
            return;
        }
        self.cursor = next_char_boundary(&self.input, self.cursor);
    }

    /// Move the cursor to the start of the input line.
    pub fn move_cursor_home(&mut self) {
        self.cursor = 0;
    }

    /// Move the cursor to the end of the input line.
    pub fn move_cursor_end(&mut self) {
        self.cursor = self.input.len();
    }

    /// Clear the input line and reset recall/candidate state.
    pub fn clear(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.input_history_index = None;
        self.draft_input.clear();
        self.selected_candidate = None;
        self.recompute_candidates();
    }

    /// Return the trimmed input and clear the line, or `None` if empty.
    pub fn take_input(&mut self) -> Option<String> {
        let text = self.input.trim();
        if text.is_empty() {
            return None;
        }
        let text = text.to_string();
        self.clear();
        Some(text)
    }

    /// Remember a submitted line for Up/Down recall.
    pub fn record_history(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if self.input_history.first().map(|s| s.as_str()) != Some(text) {
            self.input_history.insert(0, text.to_string());
        }
    }

    /// Recompute the completion candidate list based on the current input.
    pub fn recompute_candidates(&mut self) {
        self.candidates.clear();
        self.selected_candidate = None;

        if self.input.is_empty() {
            return;
        }

        if self.input.starts_with('/') {
            let candidates = BUILT_IN_REGISTRY.completion_candidates(&self.input);
            for candidate in candidates {
                if !self.candidates.contains(&candidate) {
                    self.candidates.push(candidate);
                }
            }
        } else {
            let prefix = self.input.to_lowercase();
            for entry in &self.input_history {
                if entry.to_lowercase().starts_with(&prefix) && !self.candidates.contains(entry) {
                    self.candidates.push(entry.clone());
                }
            }
        }
    }

    /// Recall the next older input from the session history (bound to Up).
    pub fn history_previous(&mut self) {
        if self.input_history.is_empty() {
            return;
        }

        match self.input_history_index {
            None => {
                self.draft_input = self.input.clone();
                self.input_history_index = Some(0);
            }
            Some(i) if i + 1 < self.input_history.len() => {
                self.input_history_index = Some(i + 1);
            }
            Some(_) => {}
        }

        if let Some(i) = self.input_history_index {
            self.input = self.input_history[i].clone();
            self.cursor = self.input.len();
        }
        self.selected_candidate = None;
        self.recompute_candidates();
    }

    /// Recall the next newer input, restoring the draft line at the top
    /// (bound to Down).
    pub fn history_next(&mut self) {
        match self.input_history_index {
            None => {}
            Some(0) => {
                self.input_history_index = None;
                self.input = self.draft_input.clone();
            }
            Some(i) => {
                self.input_history_index = Some(i - 1);
                self.input = self.input_history[i - 1].clone();
            }
        }
        self.cursor = self.input.len();
        self.selected_candidate = None;
        self.recompute_candidates();
    }

    /// Cycle through completion candidates by `delta` positions and fill the
    /// input box with the selected candidate. Wraps around at both ends.
    pub fn cycle_candidate(&mut self, delta: isize) {
        if self.candidates.is_empty() {
            return;
        }

        let idx = match self.selected_candidate {
            None if delta >= 0 => 0usize,
            None => self.candidates.len() - 1,
            Some(i) => {
                let len = self.candidates.len() as isize;
                let next = (i as isize + delta).rem_euclid(len);
                next as usize
            }
        };

        self.selected_candidate = Some(idx);
        self.input = self.candidates[idx].clone();
        self.cursor = self.input.len();
        self.input_history_index = None;
        // Candidates stay valid because the new input matches the prefix.
    }

    /// Clear the active candidate selection without changing the input.
    pub fn clear_candidate_selection(&mut self) {
        self.selected_candidate = None;
    }
}

/// Return the previous UTF-8 character boundary before `idx`.
fn prev_char_boundary(s: &str, idx: usize) -> usize {
    if idx == 0 {
        return 0;
    }
    let mut pos = idx - 1;
    while !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

/// Return the next UTF-8 character boundary at or after `idx`.
fn next_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut pos = idx + 1;
    while pos < s.len() && !s.is_char_boundary(pos) {
        pos += 1;
    }
    pos
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_resets_history_recall() {
        let mut state = InputState::new();
        state.record_history("base");
        state.history_previous();
        assert!(state.input_history_index.is_some());

        state.push_char('x');
        assert!(state.input_history_index.is_none());
        assert_eq!(state.input, "basex");
    }

    #[test]
    fn cursor_moves_and_inserts_at_cursor() {
        let mut state = InputState::new();
        state.push_char('a');
        state.push_char('b');
        state.push_char('c');
        assert_eq!(state.input, "abc");
        assert_eq!(state.cursor, 3);

        state.move_cursor_left();
        state.move_cursor_left();
        assert_eq!(state.cursor, 1);

        state.push_char('x');
        assert_eq!(state.input, "axbc");
        assert_eq!(state.cursor, 2);

        state.backspace();
        assert_eq!(state.input, "abc");
        assert_eq!(state.cursor, 1);

        state.move_cursor_home();
        assert_eq!(state.cursor, 0);
        state.move_cursor_end();
        assert_eq!(state.cursor, 3);
    }

    #[test]
    fn input_history_recalls_newest_first() {
        let mut state = InputState::new();
        state.record_history("first");
        state.record_history("second");
        state.record_history("third");

        assert_eq!(state.input_history, vec!["third", "second", "first"]);

        state.input = "draft".to_string();
        state.history_previous();
        assert_eq!(state.input, "third");
        state.history_previous();
        assert_eq!(state.input, "second");
        state.history_next();
        assert_eq!(state.input, "third");
        state.history_next();
        assert_eq!(state.input, "draft");
        assert!(state.input_history_index.is_none());
    }

    #[test]
    fn consecutive_duplicate_inputs_are_not_stored_twice() {
        let mut state = InputState::new();
        state.record_history("same");
        state.record_history("same");
        assert_eq!(state.input_history, vec!["same"]);
    }

    #[test]
    fn slash_command_completion_offers_resume() {
        let mut state = InputState::new();
        state.input = "/res".to_string();
        state.recompute_candidates();
        assert_eq!(state.candidates, vec!["/resume"]);
    }

    #[test]
    fn slash_command_completion_offers_help_and_exit() {
        let mut state = InputState::new();
        state.input = "/".to_string();
        state.recompute_candidates();
        assert!(state.candidates.contains(&"/help".to_string()));
        assert!(state.candidates.contains(&"/exit".to_string()));
    }

    #[test]
    fn slash_command_completion_offers_status() {
        let mut state = InputState::new();
        state.input = "/st".to_string();
        state.recompute_candidates();
        assert_eq!(state.candidates, vec!["/status"]);
    }

    #[test]
    fn slash_command_completion_offers_mcp_and_subcommands() {
        let mut state = InputState::new();
        state.input = "/mc".to_string();
        state.recompute_candidates();
        assert_eq!(state.candidates, vec!["/mcp", "/mcp list", "/mcp status"]);
    }

    #[test]
    fn slash_command_completion_offers_parameterized_forms() {
        let mut state = InputState::new();
        state.input = "/mcp ".to_string();
        state.recompute_candidates();
        assert_eq!(state.candidates, vec!["/mcp list", "/mcp status"]);

        state.input = "/help ".to_string();
        state.recompute_candidates();
        assert!(state.candidates.contains(&"/help config".to_string()));
        assert!(state.candidates.contains(&"/help mcp".to_string()));
        assert!(state.candidates.contains(&"/help mcp list".to_string()));
    }

    #[test]
    fn history_completion_filters_by_prefix_case_insensitively() {
        let mut state = InputState::new();
        state.record_history("Hello World");
        state.record_history("hello there");
        state.record_history("goodbye");

        state.input = "HEL".to_string();
        state.recompute_candidates();

        assert_eq!(state.candidates, vec!["hello there", "Hello World"]);
    }

    #[test]
    fn tab_cycles_through_candidates() {
        let mut state = InputState::new();
        state.candidates = vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()];

        state.cycle_candidate(1);
        assert_eq!(state.input, "alpha");
        assert_eq!(state.selected_candidate, Some(0));

        state.cycle_candidate(1);
        assert_eq!(state.input, "beta");

        state.cycle_candidate(-1);
        assert_eq!(state.input, "alpha");

        state.cycle_candidate(-1);
        assert_eq!(state.input, "gamma"); // wrap backward
    }

    #[test]
    fn history_completion_is_not_capped_to_max_candidates() {
        let mut state = InputState::new();
        for i in 0..12 {
            state.record_history(&format!("cmd{}", i));
        }
        state.input = "cmd".to_string();
        state.recompute_candidates();

        assert_eq!(
            state.candidates.len(),
            12,
            "all matching history entries should be candidates"
        );
    }
}
