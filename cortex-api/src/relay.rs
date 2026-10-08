use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::Message;
use axum::extract::ws::WebSocket;
use axum::extract::ws::WebSocketUpgrade;
use axum::http::HeaderMap;
use axum::response::Response;
use serde_json::{Value, json};
use tokio_postgres::AsyncMessage;
use tokio_postgres::NoTls;
use uuid::Uuid;

use crate::AppState;
use crate::auth::user_from_headers;

pub fn spawn_listener(database_url: String, relay: tokio::sync::broadcast::Sender<i64>) {
    tokio::spawn(async move {
        loop {
            if listen_once(&database_url, &relay).await.is_err() {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    });
}

async fn listen_once(
    database_url: &str,
    relay: &tokio::sync::broadcast::Sender<i64>,
) -> Result<(), ()> {
    let (client, mut connection) = tokio_postgres::connect(database_url, NoTls)
        .await
        .map_err(|_| ())?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let next = futures_util::future::poll_fn(|cx| connection.poll_message(cx)).await;
            match next {
                Some(Ok(AsyncMessage::Notification(note))) => {
                    if tx.send(note.payload().to_owned()).is_err() {
                        break;
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            }
        }
    });
    client.batch_execute("LISTEN relay").await.map_err(|_| ())?;
    while let Some(payload) = rx.recv().await {
        if let Ok(id) = payload.parse::<i64>() {
            let _ = relay.send(id);
        }
    }
    Err(())
}

pub async fn code_agent(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let user = match user_from_headers(&state, &headers) {
        Ok(user) => user,
        Err(response) => return response,
    };
    ws.on_upgrade(move |socket| code_socket(state, user, socket))
}

pub async fn gateway(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let user = match user_from_headers(&state, &headers) {
        Ok(user) => user,
        Err(response) => return response,
    };
    ws.on_upgrade(move |socket| gateway_socket(state, user, socket))
}

async fn code_socket(state: AppState, user: Uuid, mut socket: WebSocket) {
    let sender = Uuid::new_v4().to_string();
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "protocolVersion": "1" }
    });
    if socket.send(Message::text(init.to_string())).await.is_err() {
        return;
    }
    let mut room = None;
    let mut events = state.relay.subscribe();
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                let Some(Ok(message)) = incoming else { break };
                match message {
                    Message::Text(text) => {
                        let raw = text.to_string();
                        if let Ok(value) = serde_json::from_str::<Value>(&raw) {
                            if value.get("id").and_then(|id| id.as_i64()) == Some(1) && value.get("result").is_some() {
                                let ready = json!({"jsonrpc":"2.0","method":"_cortex/relay/initialized"});
                                if socket.send(Message::text(ready.to_string())).await.is_err() {
                                    break;
                                }
                            }
                            if let Some(session_id) = session_id(&value) {
                                room = Some(format!("sess:{session_id}"));
                                let _ = remember_session(&state, user, &session_id, &value).await;
                            }
                            if value.get("method").is_some()
                                && let Some(room) = room.as_deref()
                                && publish(&state, room, &sender, &raw).await.is_err()
                            {
                                break;
                            }
                        }
                    }
                    Message::Ping(bytes) => {
                        if socket.send(Message::Pong(bytes)).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            event = events.recv() => {
                match event {
                    Ok(id) => {
                        if let Some(frame) = frame_for(&state, id, room.as_deref(), &sender).await
                            && socket.send(Message::text(frame)).await.is_err()
                        {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

async fn gateway_socket(state: AppState, user: Uuid, mut socket: WebSocket) {
    let sender = Uuid::new_v4().to_string();
    let room = format!("user:{user}");
    let mut events = state.relay.subscribe();
    let mut greeted = false;
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                let Some(Ok(message)) = incoming else { break };
                match message {
                    Message::Text(text) => {
                        let raw = text.to_string();
                        if !greeted {
                            let ack = json!({
                                "connection_id": sender,
                                "user_id": user.to_string(),
                                "computer_hub_version": "1",
                                "supported_protocol_versions": ["1.0.0"],
                                "capabilities": []
                            });
                            if socket.send(Message::text(ack.to_string())).await.is_err() {
                                break;
                            }
                            greeted = true;
                            continue;
                        }
                        if publish(&state, &room, &sender, &raw).await.is_err() {
                            break;
                        }
                    }
                    Message::Ping(bytes) => {
                        if socket.send(Message::Pong(bytes)).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            event = events.recv() => {
                match event {
                    Ok(id) => {
                        if greeted
                            && let Some(frame) = frame_for(&state, id, Some(&room), &sender).await
                            && socket.send(Message::text(frame)).await.is_err()
                        {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

fn session_id(value: &Value) -> Option<String> {
    value
        .pointer("/params/sessionId")
        .and_then(|item| item.as_str())
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
}

async fn remember_session(
    state: &AppState,
    user: Uuid,
    session_id: &str,
    value: &Value,
) -> Result<(), ()> {
    let cwd = value
        .pointer("/params/cwd")
        .and_then(|item| item.as_str())
        .unwrap_or("");
    let client = state.db.conn().await.map_err(|_| ())?;
    client
        .execute(
            "INSERT INTO remote_sessions (user_id, session_id, cwd, status) VALUES ($1, $2, $3, 'active')
             ON CONFLICT (user_id, session_id) DO UPDATE SET cwd = EXCLUDED.cwd, updated_at = now()",
            &[&user, &session_id, &cwd],
        )
        .await
        .map_err(|err| {
            tracing::error!(sqlstate = ?err.code(), "session sync failed");
        })?;
    Ok(())
}

async fn publish(state: &AppState, room: &str, sender: &str, body: &str) -> Result<(), ()> {
    if body.len() > 1_000_000 {
        return Err(());
    }
    let client = state.db.conn().await.map_err(|_| ())?;
    let id: i64 = client
        .query_one(
            "INSERT INTO relay_events (room, sender, body) VALUES ($1, $2, $3) RETURNING id",
            &[&room, &sender, &body],
        )
        .await
        .map_err(|err| {
            tracing::error!(sqlstate = ?err.code(), "relay insert failed");
        })?
        .get(0);
    let payload = id.to_string();
    client
        .execute("SELECT pg_notify('relay', $1)", &[&payload])
        .await
        .map_err(|err| {
            tracing::error!(sqlstate = ?err.code(), "relay notify failed");
        })?;
    Ok(())
}

async fn frame_for(state: &AppState, id: i64, room: Option<&str>, sender: &str) -> Option<String> {
    let room = room?;
    let client = state.db.conn().await.ok()?;
    let row = client
        .query_opt(
            "SELECT room, sender, body FROM relay_events WHERE id = $1",
            &[&id],
        )
        .await
        .ok()??;
    let event_room: String = row.get(0);
    let event_sender: String = row.get(1);
    if event_room != room || event_sender == sender {
        return None;
    }
    Some(row.get(2))
}
