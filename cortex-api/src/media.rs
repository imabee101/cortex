//! The harness calls image, video and voice endpoints whenever its tools or dictation are used, and a client
//! signed in with an API key never reads the settings that switch them off. This server has only a language
//! model, so each call is answered in the shape the client already reports: a tool error the model reads
//! (the client keeps the first 200 characters of the body) and an STT `error` event the dictation UI shows.

use axum::body::Bytes;
use axum::extract::ws::Message;
use axum::extract::ws::WebSocket;
use axum::extract::ws::WebSocketUpgrade;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use serde_json::json;

use crate::auth::AuthUser;

const NO_MEDIA_MODEL: &str = "This server has no image or video model, so this tool cannot run here. Do not retry this tool.";
const NO_SPEECH_MODEL: &str = "This server has no speech model, so voice dictation is unavailable.";

/// The body is read before answering so a large upload ends in this answer, not a reset connection.
pub async fn unavailable(AuthUser(_user): AuthUser, _body: Bytes) -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        axum::Json(json!({"error": {
            "message": NO_MEDIA_MODEL,
            "type": "invalid_request_error",
            "code": "not_supported",
        }})),
    )
        .into_response()
}

pub async fn stt(AuthUser(_user): AuthUser, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(stt_socket)
}

async fn stt_socket(mut socket: WebSocket) {
    let error = json!({"type": "error", "message": NO_SPEECH_MODEL});
    if socket.send(Message::text(error.to_string())).await.is_ok() {
        let _ = socket.send(Message::Close(None)).await;
    }
}
