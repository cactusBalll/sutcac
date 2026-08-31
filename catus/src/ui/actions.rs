//! Actions produced by the UI layer for the main event loop to execute.

/// Outcome of handling a key or other UI event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppAction {
    /// Nothing further for the event loop to do.
    None,
    /// Request a clean shutdown.
    Quit,
    /// The user submitted a message; start streaming the assistant reply.
    StartStream,
    /// Show an error message in the status bar.
    SetError(String),
}
