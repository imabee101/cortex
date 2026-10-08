use axum::Json;
use axum::body::Bytes;
use axum::extract::Multipart;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::http::header::RETRY_AFTER;
use axum::response::IntoResponse;
use axum::response::Response;
use base64::Engine;
use chrono::Utc;
use prod_mc_cli_chat_proxy_types::{
    BatchUploadRequest, BatchUploadResponse, BatchUploadResult, BatchUploadStatus,
    SandboxEnvironment, SandboxEnvironmentResponse, SandboxEnvironmentVariable,
    SandboxEnvironmentWithMetadata, SandboxForkResponse, SandboxForkedSession,
    SandboxListEnvironmentsResponse, SignedUploadUrlResponse,
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthUser;
use crate::inference;
use crate::jwt;

const QUOTA: i64 = 256 * 1024 * 1024;
const PART: u64 = 8 * 1024 * 1024;

#[derive(Deserialize)]
pub(crate) struct SaveBody {
    messages: Vec<Value>,
    #[serde(default)]
    metadata: Option<Value>,
}

#[derive(Deserialize)]
pub(crate) struct UpsertBody {
    #[serde(default)]
    session: Value,
    #[serde(rename = "agentId", default)]
    agent_id: String,
}

#[derive(Deserialize)]
pub(crate) struct ConvQuery {
    #[serde(rename = "pageSize", default)]
    page_size: Option<i64>,
    #[serde(rename = "pageToken", default)]
    page_token: Option<String>,
    #[serde(rename = "searchQuery", default)]
    search_query: Option<String>,
    #[serde(rename = "workspaceId", default)]
    workspace_id: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ConvBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    starred: Option<bool>,
    #[serde(default)]
    workspaces: Option<Vec<WsIn>>,
}

#[derive(Deserialize)]
pub(crate) struct WsIn {
    #[serde(rename = "workspaceId")]
    workspace_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    kind: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct WsQuery {
    #[serde(rename = "pageSize", default)]
    page_size: Option<i64>,
    #[serde(rename = "pageToken", default)]
    page_token: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    kind: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct LocaleBody {
    #[serde(default)]
    #[allow(dead_code)]
    locale: String,
}

#[derive(Deserialize)]
pub(crate) struct ForkBody {
    #[serde(rename = "sourceSandboxId")]
    source_sandbox_id: String,
    #[serde(default)]
    copies: Option<u32>,
}

#[derive(Deserialize)]
pub(crate) struct EnvPage {
    #[serde(default)]
    page: Option<i32>,
    #[serde(rename = "pageSize", default)]
    page_size: Option<i32>,
}

#[derive(Deserialize)]
pub(crate) struct RegisterBody {
    #[serde(rename = "sessionId")]
    session_id: String,
    cwd: String,
    #[serde(rename = "gcsTracePrefix")]
    gcs_trace_prefix: String,
    #[serde(rename = "modelId", default)]
    model_id: Option<String>,
    #[serde(rename = "repoRemoteUrl", default)]
    repo_remote_url: Option<String>,
    #[serde(default)]
    hostname: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ReplicaUpdate {
    #[serde(default)]
    summary: Option<String>,
    #[serde(rename = "firstPrompt", default)]
    first_prompt: Option<String>,
    #[serde(rename = "lastTurnNumber", default)]
    last_turn_number: Option<i32>,
    #[serde(rename = "repoHeadAtEnd", default)]
    repo_head_at_end: Option<String>,
    #[serde(rename = "restorableTurnNumber", default)]
    restorable_turn_number: Option<i32>,
}

#[derive(Deserialize)]
pub(crate) struct SearchQuery {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

#[derive(Deserialize)]
pub(crate) struct DownloadQuery {
    file: String,
    turn: i32,
}

#[derive(Deserialize)]
pub(crate) struct ExistsBody {
    paths: Vec<String>,
}

#[derive(Deserialize)]
pub(crate) struct InitBody {
    #[serde(rename = "totalSize")]
    total_size: u64,
}

#[derive(Deserialize)]
pub(crate) struct CompleteBody {
    parts: Vec<CompletePart>,
}

#[derive(Deserialize)]
pub(crate) struct CompletePart {
    #[serde(rename = "partNumber")]
    part_number: i32,
    path: String,
}

pub async fn list_sessions(State(state): State<AppState>, AuthUser(user): AuthUser) -> Response {
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    let Ok(rows) = client
        .query(
            "SELECT session_id, title, cwd, status, metadata, created_at, updated_at FROM remote_sessions WHERE user_id = $1 ORDER BY updated_at DESC",
            &[&user],
        )
        .await
    else {
        return sql_failed();
    };
    let sessions: Vec<_> = rows.iter().map(session_value).collect();
    Json(json!({"sessions": sessions})).into_response()
}

pub async fn upsert_session(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    Json(body): Json<UpsertBody>,
) -> Response {
    let title = body.session.get("title").and_then(|value| value.as_str());
    let cwd = body.session.get("cwd").and_then(|value| value.as_str());
    let status = body
        .session
        .get("status")
        .and_then(|value| value.as_str())
        .unwrap_or("active");
    let metadata = body.session.get("metadata").cloned().unwrap_or(json!({}));
    let metadata_text = metadata.to_string();
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    let result = client
        .execute(
            "INSERT INTO remote_sessions (user_id, session_id, title, cwd, status, metadata, agent_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (user_id, session_id) DO UPDATE
             SET title = EXCLUDED.title, cwd = EXCLUDED.cwd, status = EXCLUDED.status,
                 metadata = EXCLUDED.metadata, agent_id = EXCLUDED.agent_id, updated_at = now()",
            &[&user, &id, &title, &cwd, &status, &metadata_text, &body.agent_id],
        )
        .await;
    if result.is_err() {
        return sql_failed();
    }
    StatusCode::OK.into_response()
}

pub async fn save_session(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if body.len() > 8 * 1024 * 1024 {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let bytes = match inference::decode_body(&headers, body) {
        Ok(bytes) => bytes,
        Err(()) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let Ok(parsed) = serde_json::from_slice::<SaveBody>(&bytes) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let messages = serde_json::to_string(&parsed.messages).unwrap_or_else(|_| "[]".to_owned());
    let metadata = parsed.metadata.map(|value| value.to_string());
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    let updated = client
        .execute(
            "UPDATE remote_sessions SET messages = $3, metadata = COALESCE($4, metadata), updated_at = now()
             WHERE user_id = $1 AND session_id = $2",
            &[&user, &id, &messages, &metadata],
        )
        .await;
    match updated {
        Ok(0) => StatusCode::NOT_FOUND.into_response(),
        Ok(_) => StatusCode::OK.into_response(),
        Err(_) => sql_failed(),
    }
}

pub async fn load_session(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
) -> Response {
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    let Ok(row) = client
        .query_opt(
            "SELECT session_id, title, cwd, status, metadata, messages, created_at, updated_at FROM remote_sessions WHERE user_id = $1 AND session_id = $2",
            &[&user, &id],
        )
        .await
    else {
        return sql_failed();
    };
    let Some(row) = row else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let messages_text: String = row.get(5);
    let stored: Vec<Value> = serde_json::from_str(&messages_text).unwrap_or_default();
    let messages: Vec<_> = stored
        .into_iter()
        .enumerate()
        .map(|(index, message)| {
            let timestamp = message
                .get("timestamp")
                .and_then(|value| value.as_str())
                .map(str::to_owned);
            let content = if let Some(content) = message.get("content").cloned() {
                content
            } else {
                message
            };
            json!({"id": index.to_string(), "content": content, "timestamp": timestamp})
        })
        .collect();
    let session = json!({
        "sessionId": row.get::<_, String>(0),
        "title": row.get::<_, Option<String>>(1),
        "cwd": row.get::<_, Option<String>>(2),
        "status": row.get::<_, Option<String>>(3),
        "createdAt": row.get::<_, chrono::DateTime<Utc>>(6).to_rfc3339(),
        "updatedAt": row.get::<_, chrono::DateTime<Utc>>(7).to_rfc3339(),
        "metadata": serde_json::from_str::<Value>(&row.get::<_, String>(4)).unwrap_or(json!({})),
    });
    Json(json!({"messages": messages, "session": session})).into_response()
}

pub async fn delete_session(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
) -> Response {
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    match client
        .execute(
            "DELETE FROM remote_sessions WHERE user_id = $1 AND session_id = $2",
            &[&user, &id],
        )
        .await
    {
        Ok(0) => StatusCode::NOT_FOUND.into_response(),
        Ok(_) => StatusCode::OK.into_response(),
        Err(_) => sql_failed(),
    }
}

pub async fn share_session(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
) -> Response {
    let permission = jwt::random_token();
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    match client
        .execute(
            "UPDATE remote_sessions SET share_id = $3, updated_at = now() WHERE user_id = $1 AND session_id = $2",
            &[&user, &id, &permission],
        )
        .await
    {
        Ok(0) => StatusCode::NOT_FOUND.into_response(),
        Ok(_) => Json(json!({"permissionId": permission})).into_response(),
        Err(_) => sql_failed(),
    }
}

pub async fn read_share(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
) -> Response {
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    let Ok(row) = client
        .query_opt(
            "SELECT session_id, title, cwd, status, metadata, created_at, updated_at FROM remote_sessions WHERE user_id = $1 AND share_id = $2",
            &[&user, &id],
        )
        .await
    else {
        return sql_failed();
    };
    match row {
        Some(row) => Json(session_value(&row)).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub async fn list_conversations(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Query(query): Query<ConvQuery>,
) -> Response {
    let page_size = query.page_size.unwrap_or(50).clamp(1, 100);
    let offset: i64 = query
        .page_token
        .as_deref()
        .and_then(|token| token.parse().ok())
        .unwrap_or(0)
        .max(0);
    let search = query
        .search_query
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(|value| format!("%{value}%"));
    let workspace = query
        .workspace_id
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(|value| format!("%{value}%"));
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    let Ok(rows) = client
        .query(
            "SELECT conversation_id, title, starred, workspaces, created_at, updated_at
             FROM conversations
             WHERE user_id = $1 AND deleted_at IS NULL
               AND ($2::text IS NULL OR title ILIKE $2)
               AND ($3::text IS NULL OR workspaces ILIKE $3)
             ORDER BY updated_at DESC
             LIMIT $4 OFFSET $5",
            &[&user, &search, &workspace, &(page_size + 1), &offset],
        )
        .await
    else {
        return sql_failed();
    };
    let more = rows.len() as i64 > page_size;
    let conversations: Vec<_> = rows
        .iter()
        .take(page_size as usize)
        .map(conversation_value)
        .collect();
    let next = more.then(|| (offset + page_size).to_string());
    if search.is_some() {
        let matches: Vec<_> = conversations
            .iter()
            .map(|conversation| json!({"conversation": conversation}))
            .collect();
        return Json(json!({
            "conversations": [],
            "textSearchMatches": matches,
            "nextPageToken": next,
        }))
        .into_response();
    }
    Json(json!({"conversations": conversations, "nextPageToken": next})).into_response()
}

pub async fn update_conversation(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    Json(body): Json<ConvBody>,
) -> Response {
    let title = body.title.unwrap_or_default();
    let starred = body.starred.unwrap_or(false);
    let workspaces = body.workspaces.unwrap_or_default();
    let workspace_json = serde_json::to_string(
        &workspaces
            .iter()
            .map(|item| json!({"workspaceId": item.workspace_id}))
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| "[]".to_owned());
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    if client
        .execute(
            "INSERT INTO conversations (user_id, conversation_id, title, starred, workspaces)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (user_id, conversation_id) DO UPDATE
             SET title = EXCLUDED.title, starred = EXCLUDED.starred, workspaces = EXCLUDED.workspaces,
                 deleted_at = NULL, updated_at = now()",
            &[&user, &id, &title, &starred, &workspace_json],
        )
        .await
        .is_err()
    {
        return sql_failed();
    }
    for workspace in workspaces {
        if workspace.workspace_id.is_empty() {
            continue;
        }
        let name = if workspace.name.is_empty() {
            workspace.workspace_id.clone()
        } else {
            workspace.name
        };
        if client
            .execute(
                "INSERT INTO workspaces (user_id, workspace_id, name, kind) VALUES ($1, $2, $3, $4)
                 ON CONFLICT (user_id, workspace_id) DO UPDATE SET name = EXCLUDED.name, kind = EXCLUDED.kind",
                &[&user, &workspace.workspace_id, &name, &workspace.kind],
            )
            .await
            .is_err()
        {
            return sql_failed();
        }
    }
    StatusCode::OK.into_response()
}

pub async fn delete_conversation(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
) -> Response {
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    match client
        .execute(
            "UPDATE conversations SET deleted_at = now() WHERE user_id = $1 AND conversation_id = $2 AND deleted_at IS NULL",
            &[&user, &id],
        )
        .await
    {
        Ok(0) => StatusCode::NOT_FOUND.into_response(),
        Ok(_) => StatusCode::OK.into_response(),
        Err(_) => sql_failed(),
    }
}

pub async fn list_workspaces(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Query(query): Query<WsQuery>,
) -> Response {
    let page_size = query.page_size.unwrap_or(50).clamp(1, 100);
    let offset: i64 = query
        .page_token
        .as_deref()
        .and_then(|token| token.parse().ok())
        .unwrap_or(0)
        .max(0);
    let search = query
        .query
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(|value| format!("%{value}%"));
    let kind = query.kind.filter(|value| !value.is_empty());
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    let Ok(rows) = client
        .query(
            "SELECT workspace_id, name, kind, created_at FROM workspaces
             WHERE user_id = $1 AND ($2::text IS NULL OR name ILIKE $2) AND ($3::text IS NULL OR kind = $3)
             ORDER BY created_at DESC LIMIT $4 OFFSET $5",
            &[&user, &search, &kind, &(page_size + 1), &offset],
        )
        .await
    else {
        return sql_failed();
    };
    let more = rows.len() as i64 > page_size;
    let workspaces: Vec<_> = rows
        .iter()
        .take(page_size as usize)
        .map(|row| {
            let id: String = row.get(0);
            let name: String = row.get(1);
            let kind: Option<String> = row.get(2);
            let created: chrono::DateTime<Utc> = row.get(3);
            json!({
                "workspaceId": id,
                "name": name,
                "kind": kind,
                "createTime": created.to_rfc3339(),
            })
        })
        .collect();
    Json(json!({
        "workspaces": workspaces,
        "nextPageToken": more.then(|| (offset + page_size).to_string()),
    }))
    .into_response()
}

pub async fn list_skills(
    AuthUser(_user): AuthUser,
    Json(_body): Json<LocaleBody>,
) -> impl IntoResponse {
    Json(json!({
        "skills": [{
            "index": 0,
            "name": "cortex",
            "description": "Session tools for Cortex",
            "icon": "",
            "displayName": "Cortex"
        }]
    }))
}

pub async fn list_user_skills(AuthUser(_user): AuthUser) -> impl IntoResponse {
    Json(json!({"skills": []}))
}

pub async fn list_modes(
    State(state): State<AppState>,
    AuthUser(_user): AuthUser,
    Json(_body): Json<LocaleBody>,
) -> impl IntoResponse {
    Json(json!({
        "modes": [{
            "id": state.config.model_id,
            "title": state.config.model_id,
            "description": "Local model",
            "availability": { "available": {} },
            "iconHint": "",
            "tags": []
        }],
        "defaultModeId": state.config.model_id,
    }))
}

pub async fn create_environment(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<Value>,
) -> Response {
    let id = Uuid::new_v4().to_string();
    let env = stored_env(user, &id, &body);
    let variables = body
        .get("environmentVariables")
        .cloned()
        .unwrap_or(json!([]));
    let secrets = redact_secrets(body.get("secrets"));
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    if client
        .execute(
            "INSERT INTO sandbox_environments (user_id, environment_id, body, variables, secrets) VALUES ($1, $2, $3, $4, $5)",
            &[&user, &id, &env.to_string(), &variables.to_string(), &secrets.to_string()],
        )
        .await
        .is_err()
    {
        return sql_failed();
    }
    if client
        .execute(
            "INSERT INTO sandbox_sessions (user_id, sandbox_id, environment_id) VALUES ($1, $2, $3)",
            &[&user, &id, &id],
        )
        .await
        .is_err()
    {
        return sql_failed();
    }
    Json(environment_response(&env, &variables, &secrets)).into_response()
}

pub async fn list_environments(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Query(query): Query<EnvPage>,
) -> Response {
    let page = query.page.unwrap_or(1).max(1);
    let page_size = query.page_size.unwrap_or(50).clamp(1, 100);
    let offset = (page - 1) as i64 * page_size as i64;
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    let Ok(rows) = client
        .query(
            "SELECT body, variables, secrets FROM sandbox_environments WHERE user_id = $1 ORDER BY created_at DESC LIMIT $2 OFFSET $3",
            &[&user, &(page_size as i64 + 1), &offset],
        )
        .await
    else {
        return sql_failed();
    };
    let more = rows.len() as i32 > page_size;
    let environments = rows
        .iter()
        .take(page_size as usize)
        .map(|row| {
            let body: String = row.get(0);
            let variables: String = row.get(1);
            let secrets: String = row.get(2);
            let env: Value = serde_json::from_str(&body).unwrap_or(json!({}));
            let variables: Value = serde_json::from_str(&variables).unwrap_or(json!([]));
            let secrets: Value = serde_json::from_str(&secrets).unwrap_or(json!([]));
            metadata(&env, &variables, &secrets)
        })
        .collect();
    Json(SandboxListEnvironmentsResponse {
        environments,
        page: Some(page),
        page_size: Some(page_size),
        has_more: Some(more),
    })
    .into_response()
}

pub async fn update_environment(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    let Ok(row) = client
        .query_opt(
            "SELECT body, variables, secrets FROM sandbox_environments WHERE user_id = $1 AND environment_id = $2",
            &[&user, &id],
        )
        .await
    else {
        return sql_failed();
    };
    let Some(row) = row else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let current: String = row.get(0);
    let mut env: Value = serde_json::from_str(&current).unwrap_or(json!({}));
    let mut variables: Value =
        serde_json::from_str::<Value>(&row.get::<_, String>(1)).unwrap_or(json!([]));
    let mut secrets: Value =
        serde_json::from_str::<Value>(&row.get::<_, String>(2)).unwrap_or(json!([]));
    if let (Some(dest), Some(src)) = (env.as_object_mut(), body.as_object()) {
        for (key, value) in src {
            if matches!(
                key.as_str(),
                "secrets" | "environmentVariables" | "snapshotBucket" | "environmentId" | "userId"
            ) {
                continue;
            }
            dest.insert(key.clone(), value.clone());
        }
        dest.insert("modifyTime".to_owned(), json!(Utc::now().to_rfc3339()));
    }
    if body.get("environmentVariables").is_some() {
        variables = body
            .get("environmentVariables")
            .cloned()
            .unwrap_or(json!([]));
    }
    if body.get("secrets").is_some() {
        secrets = redact_secrets(body.get("secrets"));
    }
    if client
        .execute(
            "UPDATE sandbox_environments SET body = $3, variables = $4, secrets = $5, updated_at = now()
             WHERE user_id = $1 AND environment_id = $2",
            &[&user, &id, &env.to_string(), &variables.to_string(), &secrets.to_string()],
        )
        .await
        .is_err()
    {
        return sql_failed();
    }
    Json(environment_response(&env, &variables, &secrets)).into_response()
}

pub async fn delete_environment(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
) -> Response {
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    match client
        .execute(
            "DELETE FROM sandbox_environments WHERE user_id = $1 AND environment_id = $2",
            &[&user, &id],
        )
        .await
    {
        Ok(0) => StatusCode::NOT_FOUND.into_response(),
        Ok(_) => {
            let _ = client
                .execute(
                    "DELETE FROM sandbox_sessions WHERE user_id = $1 AND environment_id = $2",
                    &[&user, &id],
                )
                .await;
            StatusCode::OK.into_response()
        }
        Err(_) => sql_failed(),
    }
}

pub async fn fork_sandbox(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    headers: HeaderMap,
    Json(body): Json<ForkBody>,
) -> Response {
    let copies = body.copies.unwrap_or(1).clamp(1, 8);
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    let Ok(found) = client
        .query_opt(
            "SELECT environment_id FROM sandbox_sessions WHERE user_id = $1 AND sandbox_id = $2",
            &[&user, &body.source_sandbox_id],
        )
        .await
    else {
        return sql_failed();
    };
    let Some(found) = found else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let environment_id: Option<String> = found.get(0);
    let Ok(token) = sandbox_token(&state, user) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let mut ids = Vec::new();
    let mut sessions = Vec::new();
    for _ in 0..copies {
        let id = Uuid::new_v4().to_string();
        if client
            .execute(
                "INSERT INTO sandbox_sessions (user_id, sandbox_id, environment_id, source_id) VALUES ($1, $2, $3, $4)",
                &[&user, &id, &environment_id, &body.source_sandbox_id],
            )
            .await
            .is_err()
        {
            return sql_failed();
        }
        let url = format!("{}/ws/code-agent", ws_base(&headers));
        ids.push(id.clone());
        sessions.push(SandboxForkedSession {
            sandbox_id: id,
            websocket_url: url,
            jwt_token: token.clone(),
        });
    }
    Json(SandboxForkResponse {
        sandbox_ids: ids,
        sessions,
    })
    .into_response()
}

pub async fn delete_sandbox(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
) -> Response {
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    match client
        .execute(
            "DELETE FROM sandbox_sessions WHERE user_id = $1 AND sandbox_id = $2",
            &[&user, &id],
        )
        .await
    {
        Ok(0) => StatusCode::NOT_FOUND.into_response(),
        Ok(_) => StatusCode::OK.into_response(),
        Err(_) => sql_failed(),
    }
}

pub async fn register_session(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<RegisterBody>,
) -> Response {
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    let result = client
        .execute(
            "INSERT INTO registry_sessions (user_id, session_id, cwd, gcs_trace_prefix, model_id, repo_remote_url, hostname)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (user_id, session_id) DO UPDATE
             SET cwd = EXCLUDED.cwd, gcs_trace_prefix = EXCLUDED.gcs_trace_prefix, model_id = EXCLUDED.model_id,
                 repo_remote_url = EXCLUDED.repo_remote_url, hostname = EXCLUDED.hostname, updated_at = now()",
            &[
                &user,
                &body.session_id,
                &body.cwd,
                &body.gcs_trace_prefix,
                &body.model_id,
                &body.repo_remote_url,
                &body.hostname,
            ],
        )
        .await;
    if result.is_err() {
        return sql_failed();
    }
    StatusCode::OK.into_response()
}

pub async fn update_replica(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    Json(body): Json<ReplicaUpdate>,
) -> Response {
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    match client
        .execute(
            "UPDATE registry_sessions SET
                summary = COALESCE($3, summary),
                first_prompt = COALESCE($4, first_prompt),
                last_turn_number = COALESCE($5, last_turn_number),
                restorable_turn_number = COALESCE($6, restorable_turn_number),
                repo_head_at_end = COALESCE($7, repo_head_at_end),
                updated_at = now(),
                last_active_at = now()
             WHERE user_id = $1 AND session_id = $2",
            &[
                &user,
                &id,
                &body.summary,
                &body.first_prompt,
                &body.last_turn_number,
                &body.restorable_turn_number,
                &body.repo_head_at_end,
            ],
        )
        .await
    {
        Ok(0) => StatusCode::NOT_FOUND.into_response(),
        Ok(_) => StatusCode::OK.into_response(),
        Err(_) => sql_failed(),
    }
}

pub async fn finalize_replica(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
) -> Response {
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    match client
        .execute(
            "UPDATE registry_sessions SET status = 'final', updated_at = now() WHERE user_id = $1 AND session_id = $2",
            &[&user, &id],
        )
        .await
    {
        Ok(0) => StatusCode::NOT_FOUND.into_response(),
        Ok(_) => StatusCode::OK.into_response(),
        Err(_) => sql_failed(),
    }
}

pub async fn search_sessions(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Query(query): Query<SearchQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(20).clamp(1, 100);
    let pattern = query
        .query
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(|value| format!("%{value}%"));
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    let Ok(rows) = client
        .query(
            "SELECT session_id, summary, first_prompt, model_id, created_at, updated_at, last_turn_number, restorable_turn_number,
                    cwd, repo_remote_url, hostname, status, gcs_trace_prefix, gcs_bucket, last_active_at
             FROM registry_sessions
             WHERE user_id = $1 AND ($2::text IS NULL OR summary ILIKE $2 OR cwd ILIKE $2 OR COALESCE(first_prompt, '') ILIKE $2)
             ORDER BY updated_at DESC LIMIT $3",
            &[&user, &pattern, &limit],
        )
        .await
    else {
        return sql_failed();
    };
    let sessions: Vec<_> = rows.iter().map(replica_value).collect();
    Json(json!({"sessions": sessions})).into_response()
}

pub async fn get_replica(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
) -> Response {
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    let Ok(row) = client
        .query_opt(
            "SELECT session_id, summary, first_prompt, model_id, created_at, updated_at, last_turn_number, restorable_turn_number,
                    cwd, repo_remote_url, hostname, status, gcs_trace_prefix, gcs_bucket, last_active_at
             FROM registry_sessions WHERE user_id = $1 AND session_id = $2",
            &[&user, &id],
        )
        .await
    else {
        return sql_failed();
    };
    match row {
        Some(row) => Json(replica_value(&row)).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub async fn download_replica(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<DownloadQuery>,
) -> Response {
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    let Ok(row) = client
        .query_opt(
            "SELECT gcs_trace_prefix FROM registry_sessions WHERE user_id = $1 AND session_id = $2",
            &[&user, &id],
        )
        .await
    else {
        return sql_failed();
    };
    let Some(row) = row else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let prefix: String = row.get(0);
    let path = format!("{prefix}/{}/{}", query.turn, query.file);
    if !object_exists(&client, user, &path).await {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(url) = mint_grant(
        &state, &client, user, &path, "get", None, None, None, &headers,
    )
    .await
    else {
        return sql_failed();
    };
    Json(json!({"downloadUrl": url, "file": query.file, "turn": query.turn})).into_response()
}

pub async fn storage_limits(AuthUser(_user): AuthUser) -> impl IntoResponse {
    Json(json!({
        "max_file_bytes": QUOTA,
        "max_untracked_bytes": QUOTA,
        "enabled": true,
    }))
}

pub async fn storage_exists(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    headers: HeaderMap,
) -> Response {
    let Some(path) = header_path(&headers) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    match object_row(&client, user, path).await {
        Some(row) => Json(upload_value(path, &row)).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub async fn storage_batch_exists(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<ExistsBody>,
) -> Response {
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    let mut exists = Vec::new();
    let mut missing = Vec::new();
    for path in body.paths {
        if clean_path(&path).is_none() {
            missing.push(path);
            continue;
        }
        if object_exists(&client, user, &path).await {
            exists.push(path);
        } else {
            missing.push(path);
        }
    }
    Json(json!({"exists": exists, "missing": missing})).into_response()
}

pub async fn storage_upload(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(path) = header_path(&headers) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_owned();
    match write_object(&state, user, path, &content_type, &body).await {
        Ok(value) => Json(value).into_response(),
        Err(response) => response,
    }
}

pub async fn storage_batch_upload(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    mut multipart: Multipart,
) -> Response {
    let mut results = Vec::new();
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        };
        let path = field
            .file_name()
            .or_else(|| field.name())
            .unwrap_or("")
            .to_owned();
        let content_type = field
            .content_type()
            .unwrap_or("application/octet-stream")
            .to_owned();
        let data = match field.bytes().await {
            Ok(data) => data,
            Err(_) => {
                results.push(error_result(path, "upload failed"));
                continue;
            }
        };
        match write_object(&state, user, &path, &content_type, &data).await {
            Ok(value) => results.push(ok_result(&value)),
            Err(_) => results.push(error_result(path, "upload failed")),
        }
    }
    Json(BatchUploadResponse { results }).into_response()
}

pub async fn storage_batch_upload_json(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let bytes = match inference::decode_body(&headers, body) {
        Ok(bytes) => bytes,
        Err(()) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let Ok(request) = serde_json::from_slice::<BatchUploadRequest>(&bytes) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let mut results = Vec::new();
    for file in request.files {
        let content_type = if file.content_type.is_empty() {
            "application/octet-stream".to_owned()
        } else {
            file.content_type.clone()
        };
        let Ok(data) = base64::engine::general_purpose::STANDARD.decode(file.data.as_bytes())
        else {
            results.push(error_result(file.path, "upload failed"));
            continue;
        };
        match write_object(&state, user, &file.path, &content_type, &data).await {
            Ok(value) => results.push(ok_result(&value)),
            Err(_) => results.push(error_result(file.path, "upload failed")),
        }
    }
    Json(BatchUploadResponse { results }).into_response()
}

pub async fn storage_download(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    headers: HeaderMap,
) -> Response {
    let Some(path) = header_path(&headers) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    if !object_exists(&client, user, path).await {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(url) = mint_grant(
        &state, &client, user, path, "get", None, None, None, &headers,
    )
    .await
    else {
        return sql_failed();
    };
    Json(json!({"signed_url": url})).into_response()
}

pub async fn signed_upload(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    headers: HeaderMap,
) -> Response {
    let Some(path) = header_path(&headers) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_owned();
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    let Some(url) = mint_grant(
        &state,
        &client,
        user,
        path,
        "put",
        Some(&content_type),
        None,
        None,
        &headers,
    )
    .await
    else {
        return sql_failed();
    };
    Json(SignedUploadUrlResponse {
        signed_url: url,
        bucket: "cortex".to_owned(),
        path: path.to_owned(),
        content_type,
        expires_in_secs: 3600,
    })
    .into_response()
}

pub async fn multipart_init(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    headers: HeaderMap,
    Json(body): Json<InitBody>,
) -> Response {
    let upload_id = Uuid::new_v4().to_string();
    let parts = body.total_size.div_ceil(PART).clamp(1, 100);
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    if client
        .execute(
            "INSERT INTO multipart_uploads (upload_id, user_id, total_size) VALUES ($1, $2, $3)",
            &[&upload_id, &user, &(body.total_size as i64)],
        )
        .await
        .is_err()
    {
        return sql_failed();
    }
    let mut part_urls = Vec::new();
    for number in 1..=parts {
        let path = format!("multipart/{upload_id}/{number}");
        let Some(url) = mint_grant(
            &state,
            &client,
            user,
            &path,
            "part",
            None,
            Some(&upload_id),
            Some(number as i32),
            &headers,
        )
        .await
        else {
            return sql_failed();
        };
        part_urls.push(json!({
            "partNumber": number,
            "url": url,
            "path": path,
        }));
    }
    Json(json!({
        "uploadId": upload_id,
        "bucket": "cortex",
        "maxPartSizeBytes": PART,
        "partUrls": part_urls,
        "expiresAt": (Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
    }))
    .into_response()
}

pub async fn multipart_complete(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // The harness sends the object content type in Content-Type and a JSON body.
    let Ok(body) = serde_json::from_slice::<CompleteBody>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(path) = header_path(&headers) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_owned();
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    let Ok(owner) = client
        .query_opt(
            "SELECT user_id FROM multipart_uploads WHERE upload_id = $1",
            &[&id],
        )
        .await
    else {
        return sql_failed();
    };
    let Some(owner) = owner else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let owner_id: Uuid = owner.get(0);
    if owner_id != user {
        return StatusCode::NOT_FOUND.into_response();
    }
    let mut bytes = Vec::new();
    for part in &body.parts {
        let Ok(row) = client
            .query_opt(
                "SELECT content FROM multipart_parts WHERE upload_id = $1 AND part_number = $2",
                &[&id, &part.part_number],
            )
            .await
        else {
            return sql_failed();
        };
        let Some(row) = row else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let content: Vec<u8> = row.get(0);
        bytes.extend(content);
        let _ = part.path;
    }
    match write_object(&state, user, path, &content_type, &bytes).await {
        Ok(value) => Json(json!({
            "bucket": value["bucket"],
            "path": value["path"],
            "gcsUrl": format!("cortex://{}", path),
            "size": value["size"],
            "partsComposed": body.parts.len(),
            "generation": value["generation"],
        }))
        .into_response(),
        Err(response) => response,
    }
}

pub async fn storage_put(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    let Some(grant) = grant_row(&client, &token, "put").await else {
        return StatusCode::FORBIDDEN.into_response();
    };
    let content_type = grant.content_type.unwrap_or_else(|| {
        headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_owned()
    });
    match write_object(&state, grant.user_id, &grant.path, &content_type, &body).await {
        Ok(_) => {
            let _ = client
                .execute(
                    "DELETE FROM storage_grants WHERE token_hash = $1",
                    &[&jwt::sha256_hex(token.as_bytes())],
                )
                .await;
            StatusCode::OK.into_response()
        }
        Err(response) => response,
    }
}

pub async fn storage_get(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    let Some(grant) = grant_row(&client, &token, "get").await else {
        return StatusCode::FORBIDDEN.into_response();
    };
    let Some(row) = object_row(&client, grant.user_id, &grant.path).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let content: Vec<u8> = row.get(0);
    let content_type: String = row.get(1);
    ([(CONTENT_TYPE, content_type)], content).into_response()
}

pub async fn storage_part(
    State(state): State<AppState>,
    Path(token): Path<String>,
    body: Bytes,
) -> Response {
    let Ok(client) = state.db.conn().await else {
        return sql_unavailable();
    };
    let Some(grant) = grant_row(&client, &token, "part").await else {
        return StatusCode::FORBIDDEN.into_response();
    };
    let Some(upload_id) = grant.upload_id else {
        return StatusCode::FORBIDDEN.into_response();
    };
    let Some(part_number) = grant.part_number else {
        return StatusCode::FORBIDDEN.into_response();
    };
    let content = body.to_vec();
    if client
        .execute(
            "INSERT INTO multipart_parts (upload_id, part_number, path, content) VALUES ($1, $2, $3, $4)
             ON CONFLICT (upload_id, part_number) DO UPDATE SET content = EXCLUDED.content, path = EXCLUDED.path",
            &[&upload_id, &part_number, &grant.path, &content],
        )
        .await
        .is_err()
    {
        return sql_failed();
    }
    let _ = client
        .execute(
            "DELETE FROM storage_grants WHERE token_hash = $1",
            &[&jwt::sha256_hex(token.as_bytes())],
        )
        .await;
    StatusCode::OK.into_response()
}

pub(crate) struct Grant {
    user_id: Uuid,
    path: String,
    content_type: Option<String>,
    upload_id: Option<String>,
    part_number: Option<i32>,
}

async fn grant_row(client: &deadpool_postgres::Object, token: &str, op: &str) -> Option<Grant> {
    let hash = jwt::sha256_hex(token.as_bytes());
    let row = client
        .query_opt(
            "SELECT user_id, path, content_type, upload_id, part_number FROM storage_grants
             WHERE token_hash = $1 AND op = $2 AND expires_at > now()",
            &[&hash, &op],
        )
        .await
        .ok()??;
    Some(Grant {
        user_id: row.get(0),
        path: row.get(1),
        content_type: row.get(2),
        upload_id: row.get(3),
        part_number: row.get(4),
    })
}

async fn mint_grant(
    _state: &AppState,
    client: &deadpool_postgres::Object,
    user: Uuid,
    path: &str,
    op: &str,
    content_type: Option<&str>,
    upload_id: Option<&str>,
    part_number: Option<i32>,
    headers: &HeaderMap,
) -> Option<String> {
    let token = jwt::random_token();
    let hash = jwt::sha256_hex(token.as_bytes());
    let content_type = content_type.map(str::to_owned);
    let upload_id = upload_id.map(str::to_owned);
    client
        .execute(
            "INSERT INTO storage_grants (token_hash, user_id, path, op, content_type, upload_id, part_number, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, now() + interval '1 hour')",
            &[&hash, &user, &path, &op, &content_type, &upload_id, &part_number],
        )
        .await
        .map_err(|err| tracing::error!(sqlstate = ?err.code(), "grant failed"))
        .ok()?;
    let root = http_base(headers);
    let route = match op {
        "put" => "put",
        "part" => "part",
        _ => "get",
    };
    Some(format!("{root}/v1/storage/{route}/{token}"))
}

async fn write_object(
    state: &AppState,
    user: Uuid,
    path: &str,
    content_type: &str,
    bytes: &[u8],
) -> Result<Value, Response> {
    let Some(path) = clean_path(path) else {
        return Err(StatusCode::BAD_REQUEST.into_response());
    };
    let Ok(client) = state.db.conn().await else {
        return Err(sql_unavailable());
    };
    let used = client
        .query_one(
            "SELECT COALESCE(SUM(octet_length(content)), 0)::bigint FROM storage_objects WHERE user_id = $1 AND path <> $2",
            &[&user, &path],
        )
        .await
        .map_err(|_| sql_failed())?;
    let used: i64 = used.get(0);
    if used.saturating_add(bytes.len() as i64) > QUOTA {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            [(RETRY_AFTER, "60")],
            Json(json!({"error": "storage quota"})),
        )
            .into_response());
    }
    let generation = Utc::now().timestamp_millis();
    let content = bytes.to_vec();
    client
        .execute(
            "INSERT INTO storage_objects (user_id, path, content, content_type, generation)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (user_id, path) DO UPDATE
             SET content = EXCLUDED.content, content_type = EXCLUDED.content_type, generation = EXCLUDED.generation",
            &[&user, &path, &content, &content_type, &generation],
        )
        .await
        .map_err(|_| sql_failed())?;
    Ok(json!({
        "bucket": "cortex",
        "path": path,
        "size": bytes.len() as i64,
        "content_type": content_type,
        "generation": generation,
    }))
}

async fn object_exists(client: &deadpool_postgres::Object, user: Uuid, path: &str) -> bool {
    client
        .query_opt(
            "SELECT 1 FROM storage_objects WHERE user_id = $1 AND path = $2",
            &[&user, &path],
        )
        .await
        .ok()
        .flatten()
        .is_some()
}

async fn object_row(
    client: &deadpool_postgres::Object,
    user: Uuid,
    path: &str,
) -> Option<tokio_postgres::Row> {
    client
        .query_opt(
            "SELECT content, content_type, generation FROM storage_objects WHERE user_id = $1 AND path = $2",
            &[&user, &path],
        )
        .await
        .ok()
        .flatten()
}

fn upload_value(path: &str, row: &tokio_postgres::Row) -> Value {
    let content: Vec<u8> = row.get(0);
    let content_type: String = row.get(1);
    let generation: i64 = row.get(2);
    json!({
        "bucket": "cortex",
        "path": path,
        "size": content.len() as i64,
        "content_type": content_type,
        "generation": generation,
    })
}

fn ok_result(value: &Value) -> BatchUploadResult {
    BatchUploadResult {
        path: value
            .get("path")
            .and_then(|item| item.as_str())
            .unwrap_or("")
            .to_owned(),
        status: BatchUploadStatus::Ok,
        bucket: Some("cortex".to_owned()),
        size: value.get("size").and_then(|item| item.as_i64()),
        generation: value.get("generation").and_then(|item| item.as_i64()),
        error: None,
    }
}

fn error_result(path: String, error: &str) -> BatchUploadResult {
    BatchUploadResult {
        path,
        status: BatchUploadStatus::Error,
        bucket: None,
        size: None,
        generation: None,
        error: Some(error.to_owned()),
    }
}

fn header_path(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("x-storage-path")
        .and_then(|value| value.to_str().ok())
        .and_then(clean_path)
}

fn clean_path(path: &str) -> Option<&str> {
    if path.is_empty() || path.starts_with('/') || path.split('/').any(|part| part == "..") {
        None
    } else {
        Some(path)
    }
}

fn http_base(headers: &HeaderMap) -> String {
    let host = headers
        .get("host")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("llm.imabee.com");
    if host.starts_with("127.0.0.1") || host.starts_with("localhost") {
        format!("http://{host}")
    } else {
        format!("https://{host}")
    }
}

fn ws_base(headers: &HeaderMap) -> String {
    let host = headers
        .get("host")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("llm.imabee.com");
    if host.starts_with("127.0.0.1") || host.starts_with("localhost") {
        format!("ws://{host}")
    } else {
        format!("wss://{host}")
    }
}

fn session_value(row: &tokio_postgres::Row) -> Value {
    let id: String = row.get(0);
    let title: Option<String> = row.get(1);
    let cwd: Option<String> = row.get(2);
    let status: Option<String> = row.get(3);
    let metadata: String = row.get(4);
    let created: chrono::DateTime<Utc> = row.get(5);
    let updated: chrono::DateTime<Utc> = row.get(6);
    json!({
        "sessionId": id,
        "title": title,
        "cwd": cwd,
        "status": status,
        "createdAt": created.to_rfc3339(),
        "updatedAt": updated.to_rfc3339(),
        "metadata": serde_json::from_str::<Value>(&metadata).unwrap_or(json!({})),
    })
}

fn conversation_value(row: &tokio_postgres::Row) -> Value {
    let id: String = row.get(0);
    let title: String = row.get(1);
    let starred: bool = row.get(2);
    let workspaces: String = row.get(3);
    let created: chrono::DateTime<Utc> = row.get(4);
    let updated: chrono::DateTime<Utc> = row.get(5);
    json!({
        "conversationId": id,
        "title": title,
        "starred": starred,
        "createTime": created.to_rfc3339(),
        "modifyTime": updated.to_rfc3339(),
        "workspaces": serde_json::from_str::<Value>(&workspaces).unwrap_or(json!([])),
    })
}

fn replica_value(row: &tokio_postgres::Row) -> Value {
    let session_id: String = row.get(0);
    let summary: String = row.get(1);
    let first_prompt: Option<String> = row.get(2);
    let model_id: Option<String> = row.get(3);
    let created: chrono::DateTime<Utc> = row.get(4);
    let updated: chrono::DateTime<Utc> = row.get(5);
    let last_turn: i32 = row.get(6);
    let restorable: Option<i32> = row.get(7);
    let cwd: String = row.get(8);
    let repo: Option<String> = row.get(9);
    let hostname: Option<String> = row.get(10);
    let status: String = row.get(11);
    let prefix: String = row.get(12);
    let bucket: String = row.get(13);
    let active: Option<chrono::DateTime<Utc>> = row.get(14);
    json!({
        "sessionId": session_id,
        "summary": summary,
        "firstPrompt": first_prompt,
        "modelId": model_id,
        "createdAt": created.to_rfc3339(),
        "updatedAt": updated.to_rfc3339(),
        "lastTurnNumber": last_turn,
        "restorableTurnNumber": restorable,
        "cwd": cwd,
        "repoRemoteUrl": repo,
        "hostname": hostname,
        "status": status,
        "gcsTracePrefix": prefix,
        "gcsBucket": bucket,
        "lastActiveAt": active.map(|time| time.to_rfc3339()),
    })
}

fn stored_env(user: Uuid, id: &str, body: &Value) -> Value {
    let now = Utc::now().to_rfc3339();
    let mut env = json!({
        "environmentId": id,
        "userId": user.to_string(),
        "createTime": now,
        "modifyTime": now,
        "preinstalledPackages": {},
    });
    if let (Some(dest), Some(src)) = (env.as_object_mut(), body.as_object()) {
        for (key, value) in src {
            if matches!(
                key.as_str(),
                "secrets" | "environmentVariables" | "snapshotBucket"
            ) {
                continue;
            }
            dest.insert(key.clone(), value.clone());
        }
    }
    env["environmentId"] = json!(id);
    env["userId"] = json!(user.to_string());
    env
}

fn redact_secrets(value: Option<&Value>) -> Value {
    let Some(items) = value.and_then(|value| value.as_array()) else {
        return json!([]);
    };
    json!(
        items
            .iter()
            .map(|item| json!({
                "key": item.get("key").cloned().unwrap_or(Value::Null),
                "value": "",
            }))
            .collect::<Vec<_>>()
    )
}

fn environment_response(
    env: &Value,
    variables: &Value,
    secrets: &Value,
) -> SandboxEnvironmentResponse {
    SandboxEnvironmentResponse {
        environment: Some(metadata(env, variables, secrets)),
    }
}

fn metadata(env: &Value, variables: &Value, secrets: &Value) -> SandboxEnvironmentWithMetadata {
    let environment = serde_json::from_value::<SandboxEnvironment>(env.clone()).unwrap_or_default();
    SandboxEnvironmentWithMetadata {
        environment: Some(environment),
        environment_variables: serde_json::from_value::<Vec<SandboxEnvironmentVariable>>(
            variables.clone(),
        )
        .unwrap_or_default(),
        secrets: serde_json::from_value::<Vec<SandboxEnvironmentVariable>>(secrets.clone())
            .unwrap_or_default(),
        user_role: Some("owner".to_owned()),
    }
}

fn sandbox_token(state: &AppState, user: Uuid) -> Result<String, ()> {
    let now = Utc::now().timestamp();
    let keys = state.keys();
    jwt::sign(
        &keys.active.private,
        &keys.active.kid,
        &json!({
            "iss": crate::ISSUER,
            "sub": user.to_string(),
            "aud": crate::CLIENT_ID,
            "iat": now,
            "exp": now + jwt::ACCESS_TTL_SECS,
            "scope": "openid",
            "principal_type": "User",
            "principal_id": user.to_string(),
        }),
    )
    .map_err(|_| ())
}

fn sql_unavailable() -> Response {
    StatusCode::SERVICE_UNAVAILABLE.into_response()
}

fn sql_failed() -> Response {
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}
