//! Shared server state: the command channel to the runtime actor, the
//! event broadcast for WebSocket clients, and the static assets mode.

use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};

use crate::actor::Command;
use crate::assets::StaticAssets;
use catus_core::app::{AppSnapshot, RuntimeEvent};

/// An event pushed to every connected WebSocket client.
#[derive(Debug, Clone)]
pub enum BroadcastEvent {
    /// A serialized `RuntimeEvent` (`{type: "stream_text", ...}`).
    RuntimeEvent(RuntimeEvent),
    /// A full `AppSnapshot` after structural state changes.
    Snapshot(AppSnapshot),
    /// Runtime bootstrap failed; the session cannot be served.
    StartupError(String),
    /// The session requested a shutdown (`/exit`).
    AppQuit,
}

/// State shared by all HTTP/WS handlers.
pub struct AppState {
    /// Frontend actions, executed by the actor against the owned `Runtime`.
    pub cmd: mpsc::Sender<Command>,
    /// Runtime events/snapshots broadcast to WebSocket clients.
    pub events: broadcast::Sender<BroadcastEvent>,
    /// Where static UI assets are served from (overrides the embedded
    /// `web/dist` bundle; useful for development).
    pub static_assets: StaticAssets,
}

pub type SharedState = Arc<AppState>;

#[cfg(test)]
mod tests {
    use crate::BroadcastEvent;

    #[test]
    fn broadcast_event_is_debuggable_and_clonable() {
        let event = BroadcastEvent::StartupError("boom".to_string());
        let cloned = event.clone();
        let _ = format!("{:?} {:?}", event, cloned);
    }
}
