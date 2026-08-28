//! TUI rendering layer.
//!
//! Split into two concerns:
//! - [`chat`]: the always-visible chat view (history, input, candidates,
//!   status bar).
//! - [`overlay`]: modal pages rendered on top of the chat view (history
//!   picker, token usage details).
//!
//! Rendering functions are mostly pure views over [`App`]; interaction logic
//! lives in `crate::app`. The chat view may normalize `App::scroll` so that
//! it always stays within the available content range.

pub mod chat;
pub mod overlay;

use ratatui::Frame;

use crate::app::App;

/// Draw the full UI into the provided frame: the chat view plus any active
/// overlay on top of it.
pub fn draw(frame: &mut Frame, app: &mut App) {
    chat::draw_chat(frame, app);
    if app.overlay_active() {
        overlay::draw_overlay(frame, &*app);
    }
}
