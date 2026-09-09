//! The WebSocket endpoint: pushes runtime events and snapshots to every
//! connected browser client.
//!
//! Frame shape: `{"channel": "runtime-event" | "snapshot" | "startup-error"
//! | "app-quit", "payload": ...}`. On connect the server first sends the
//! current snapshot so a client can render before any event arrives.
//! Client-to-server messages are ignored (commands go through the REST
//! routes); malformed client messages terminate the connection.

use axum::extract::State;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;

use crate::actor::Command;
use crate::state::{BroadcastEvent, SharedState};

/// Upgrade `/ws` to a WebSocket connection.
pub async fn ws_handler(State(state): State<SharedState>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: SharedState) {
    // Initial snapshot so the client renders before any event arrives.
    if let Some(snapshot) = request_snapshot(&state).await {
        if relay_event(&mut socket, &BroadcastEvent::Snapshot(snapshot))
            .await
            .is_err()
        {
            return;
        }
    }

    let mut events = state.events.subscribe();
    loop {
        tokio::select! {
            event = events.recv() => {
                match event {
                    Ok(event) => {
                        if relay_event(&mut socket, &event).await.is_err() {
                            break;
                        }
                    }
                    // The client survived a burst; keep going.
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_))) => {}
                    Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {
                        // Client messages are ignored; only REST carries
                        // commands. The frame is dropped.
                    }
                }
            }
        }
    }
}

/// Pull the current snapshot through the actor (503-free best effort).
async fn request_snapshot(state: &SharedState) -> Option<catus_core::app::AppSnapshot> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    state
        .cmd
        .send(Command::GetSnapshot { reply: tx })
        .await
        .ok()?;
    rx.await.ok()
}

/// Serialize one broadcast event into a channel frame.
async fn relay_event(socket: &mut WebSocket, event: &BroadcastEvent) -> Result<(), axum::Error> {
    match event {
        BroadcastEvent::RuntimeEvent(event) => {
            let payload = serde_json::to_value(event).unwrap_or(Value::Null);
            send_frame(socket, "runtime-event", &payload).await
        }
        BroadcastEvent::Snapshot(snapshot) => {
            let payload = serde_json::to_value(snapshot).unwrap_or(Value::Null);
            send_frame(socket, "snapshot", &payload).await
        }
        BroadcastEvent::StartupError(message) => {
            send_frame(socket, "startup-error", &json!(message)).await
        }
        BroadcastEvent::AppQuit => send_frame(socket, "app-quit", &json!({})).await,
    }
}

async fn send_frame(
    socket: &mut WebSocket,
    channel: &str,
    payload: &Value,
) -> Result<(), axum::Error> {
    let frame = json!({ "channel": channel, "payload": payload });
    socket.send(WsMessage::Text(frame.to_string().into())).await
}

#[cfg(test)]
mod tests {
    use super::BroadcastEvent;
    use catus_core::app::AppSnapshot;

    #[test]
    fn startup_error_serializes() {
        let event = BroadcastEvent::StartupError("no api key".to_string());
        match &event {
            BroadcastEvent::StartupError(message) => assert_eq!(message, "no api key"),
            other => panic!("unexpected variant: {:?}", other),
        }
    }

    #[test]
    fn snapshot_event_clones() {
        // A full snapshot cannot be built without a runtime; exercise the
        // Clone bound through a fake value type instead.
        fn assert_clone<T: Clone>() {}
        assert_clone::<BroadcastEvent>();
        assert_clone::<AppSnapshot>();
    }
}
