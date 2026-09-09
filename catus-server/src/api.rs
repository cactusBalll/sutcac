//! REST command routes, mirroring the seven Tauri commands of
//! `catus-web/src-tauri/src/lib.rs` one to one. Commands are forwarded to
//! the actor over an mpsc channel and answered on a oneshot channel.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tokio::sync::oneshot;

use crate::actor::Command;
use crate::state::SharedState;
use catus_core::app::{AppSnapshot, InputLineOutcome};
use catus_core::message::Message;
use catus_core::tool::AskAnswer;

/// A request could not be served: either the command could not reach the
/// actor (startup failed or process quit → 503) or it was rejected
/// (validation error → 400).
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn actor_stopped() -> Self {
        ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "runtime actor stopped".to_string(),
        }
    }

    fn bad_request(message: String) -> Self {
        ApiError {
            status: StatusCode::BAD_REQUEST,
            message,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, self.message).into_response()
    }
}

#[derive(Deserialize)]
pub struct SendInputBody {
    pub text: String,
}

#[derive(Deserialize)]
pub struct CompleteInteractionBody {
    pub answers: Vec<AskAnswer>,
}

#[derive(Deserialize)]
pub struct OverlayActionBody {
    pub action: String,
    pub value: String,
}

#[derive(Deserialize)]
pub struct SetConfigFieldBody {
    pub scope: catus_core::config::ConfigScope,
    pub key: String,
    pub value: String,
}

#[derive(Deserialize)]
pub struct RemoveConfigFieldBody {
    pub scope: catus_core::config::ConfigScope,
    pub key: String,
}

/// Slash-command completion candidates for the current input prefix.
///
/// Served directly from the static `BUILT_IN_REGISTRY` (a pure function of
/// the input; no actor round-trip needed). Non-slash input returns an empty
/// list; the frontend completes plain text from its own input history.
pub async fn completion_candidates(Query(params): Query<CompletionQuery>) -> Json<Vec<String>> {
    let candidates =
        catus_core::app::commands::BUILT_IN_REGISTRY.completion_candidates(&params.input);
    Json(candidates)
}

#[derive(Deserialize)]
pub struct CompletionQuery {
    pub input: String,
}

/// Submit one input line (a user message or a slash command).
pub async fn send_input(
    State(state): State<SharedState>,
    body: Result<Json<SendInputBody>, JsonRejection>,
) -> Result<Json<InputLineOutcome>, ApiError> {
    let Json(body) = json_body(body)?;
    let (tx, rx) = oneshot::channel();
    dispatch(
        &state,
        Command::SendInput {
            text: body.text,
            reply: tx,
        },
    )
    .await?;
    rx.await.map(Json).map_err(|_| ApiError::actor_stopped())
}

/// Answer the paused `ask_user`/`ask_permission` interaction and resume the
/// turn. Returns whether an interaction was actually completed.
pub async fn complete_interaction(
    State(state): State<SharedState>,
    body: Result<Json<CompleteInteractionBody>, JsonRejection>,
) -> Result<Json<bool>, ApiError> {
    let Json(body) = json_body(body)?;
    let (tx, rx) = oneshot::channel();
    dispatch(
        &state,
        Command::CompleteInteraction {
            answers: body.answers,
            reply: tx,
        },
    )
    .await?;
    rx.await.map(Json).map_err(|_| ApiError::actor_stopped())
}

/// Cancel the paused interaction, reporting "user cancelled" to the model.
pub async fn cancel_interaction(State(state): State<SharedState>) -> Result<Json<bool>, ApiError> {
    let (tx, rx) = oneshot::channel();
    dispatch(&state, Command::CancelInteraction { reply: tx }).await?;
    rx.await.map(Json).map_err(|_| ApiError::actor_stopped())
}

/// Apply a picker action (`resume_history`, `activate_skill`,
/// `switch_model`, `close_subagent`, `watch_subagent`). Returns the status
/// message.
pub async fn overlay_action(
    State(state): State<SharedState>,
    body: Result<Json<OverlayActionBody>, JsonRejection>,
) -> Result<Json<String>, ApiError> {
    let Json(body) = json_body(body)?;
    let (tx, rx) = oneshot::channel();
    dispatch(
        &state,
        Command::OverlayAction {
            action: body.action,
            value: body.value,
            reply: tx,
        },
    )
    .await?;
    rx.await.map(Json).map_err(|_| ApiError::actor_stopped())
}

/// Pull the full serializable session state.
pub async fn get_snapshot(State(state): State<SharedState>) -> Result<Json<AppSnapshot>, ApiError> {
    let (tx, rx) = oneshot::channel();
    dispatch(&state, Command::GetSnapshot { reply: tx }).await?;
    rx.await.map(Json).map_err(|_| ApiError::actor_stopped())
}

/// Pull one subagent's messages for the monitor page.
pub async fn get_subagent_messages(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<Message>>, ApiError> {
    let (tx, rx) = oneshot::channel();
    dispatch(&state, Command::GetSubagentMessages { id, reply: tx }).await?;
    rx.await.map(Json).map_err(|_| ApiError::actor_stopped())
}

/// Persist the session and quit the application (the `/exit` analogue).
pub async fn quit_app(State(state): State<SharedState>) -> Result<StatusCode, ApiError> {
    dispatch(&state, Command::Shutdown).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Set a config field in one scope (`workspace` or `global`). Returns the
/// status message (400 with the reason on validation errors).
pub async fn set_config_field(
    State(state): State<SharedState>,
    body: Result<Json<SetConfigFieldBody>, JsonRejection>,
) -> Result<Json<String>, ApiError> {
    let Json(body) = json_body(body)?;
    let (tx, rx) = oneshot::channel();
    dispatch(
        &state,
        Command::SetConfigField {
            scope: body.scope,
            key: body.key,
            value: body.value,
            reply: tx,
        },
    )
    .await?;
    rx.await
        .map_err(|_| ApiError::actor_stopped())?
        .map(Json)
        .map_err(ApiError::bad_request)
}

/// Remove a config field from one scope. Returns the status message.
pub async fn remove_config_field(
    State(state): State<SharedState>,
    body: Result<Json<RemoveConfigFieldBody>, JsonRejection>,
) -> Result<Json<String>, ApiError> {
    let Json(body) = json_body(body)?;
    let (tx, rx) = oneshot::channel();
    dispatch(
        &state,
        Command::RemoveConfigField {
            scope: body.scope,
            key: body.key,
            reply: tx,
        },
    )
    .await?;
    rx.await
        .map_err(|_| ApiError::actor_stopped())?
        .map(Json)
        .map_err(ApiError::bad_request)
}

/// Forward a command to the actor, reporting a 503 when it is gone.
async fn dispatch(state: &SharedState, command: Command) -> Result<(), ApiError> {
    state
        .cmd
        .send(command)
        .await
        .map_err(|_| ApiError::actor_stopped())
}

/// Map JSON body parse failures onto a 400 response.
fn json_body<T>(body: Result<Json<T>, JsonRejection>) -> Result<Json<T>, ApiError> {
    body.map_err(|rejection| ApiError::bad_request(rejection.body_text()))
}

#[cfg(test)]
mod tests {
    use super::{CompleteInteractionBody, OverlayActionBody, SendInputBody};

    #[test]
    fn request_bodies_deserialize() {
        let input: SendInputBody =
            serde_json::from_str(r#"{"text": "hello"}"#).expect("send input");
        assert_eq!(input.text, "hello");

        let overlay: OverlayActionBody =
            serde_json::from_str(r#"{"action": "switch_model", "value": "gpt"}"#)
                .expect("overlay action");
        assert_eq!(overlay.value, "gpt");

        let answers: CompleteInteractionBody =
            serde_json::from_str(r#"{"answers": [{"prompt": "q", "answer": "a"}]}"#)
                .expect("answers");
        assert_eq!(answers.answers.len(), 1);
    }
}
