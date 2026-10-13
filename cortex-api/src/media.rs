//! The harness calls image, video and voice endpoints whenever its tools or dictation are used. Settings switch
//! them off for a signed-in session, but a client using an API key never fetches settings, so its calls arrive
//! here. This server has only a language model, so each call is answered with that fact, in the shape the client
//! already reports: an error body the tool hands the model, and an STT `error` event the dictation UI shows.

use axum::body::Bytes;
use axum::extract::ws::Message;
use axum::extract::ws::WebSocket;
use axum::extract::ws::WebSocketUpgrade;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use serde_json::json;

use crate::auth::AuthUser;

const NO_MEDIA_MODEL: &str = "this server has no image or video generation model";
const NO_SPEECH_MODEL: &str = "this server has no speech-to-text model";

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
