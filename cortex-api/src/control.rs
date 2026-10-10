use axum::Json;
use axum::body::Bytes;
use axum::extract::Form;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header::HeaderName;
use axum::response::IntoResponse;
use axum::response::Response;
use base64::Engine;
use chrono::Utc;
use prod_mc_cli_chat_proxy_types::{
    CreateFeedbackRequestInput, CreateFeedbackRequestResponse, FeedbackHeuristicsConfig,
    FeedbackRequestStatus, FeedbackRequestUpdateResponse, FeedbackResponse, FeedbackSubmission,
    SessionSignalsUpdate, SessionSignalsUpdateResponse, SessionTurnDelta, SessionTurnDeltaResponse,
    SubagentBundle,
};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::API_KEY_PREFIX;
use crate::auth::{AuthUser, Caller};
use crate::db::DbError;
use crate::{AppState, CLIENT_ID, ISSUER};

#[derive(Deserialize)]
pub struct UserQuery {
    #[serde(default)]
    include: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct PrivacyBody {
    #[serde(rename = "codingDataRetentionOptOut")]
    coding_data_retention_opt_out: bool,
}

pub async fn healthz() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

pub async fn readyz(State(state): State<AppState>) -> Response {
    if state.draining.load(std::sync::atomic::Ordering::Relaxed) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "draining"})),
        )
            .into_response();
    }
    match state.db.ping().await {
        Ok(()) => Json(json!({"status": "ok"})).into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "unavailable"})),
        )
            .into_response(),
    }
}

pub async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    use std::sync::atomic::Ordering::Relaxed;
    let body = format!(
        "cortex_api_requests_total {}\n\
         cortex_api_throttled_total {}\n\
         cortex_api_inference_slots {}\n\
         cortex_api_inference_in_flight {}\n\
         cortex_api_inference_waiting {}\n\
         cortex_api_inference_rejected_total {}\n\
         cortex_api_loops_aborted_total {}\n\
         cortex_api_reasoning_overruns_total {}\n\
         cortex_api_llama_breaker_open {}\n\
         cortex_api_search_breaker_open {}\n",
        state.requests.load(Relaxed),
        state.throttled.load(Relaxed),
        state.slots.capacity(),
        state.slots.in_flight(),
        state.slots.waiting(),
        state.slots.rejected.load(Relaxed),
        state.loops_aborted.load(Relaxed),
        state.reasoning_overruns.load(Relaxed),
        u8::from(state.llama_breaker.is_open()),
        u8::from(state.search.breaker.is_open()),
    );
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        body,
    )
}

pub async fn login_config() -> impl IntoResponse {
    Json(json!({}))
}

pub async fn settings(
    State(state): State<AppState>,
    AuthUser(_user): AuthUser,
) -> impl IntoResponse {
    let mut body = json!({
        "allow_access": true,
        "image_gen_enabled": false,
        "video_gen_enabled": false,
        "voice_mode_enabled": false,
        // The client's fetch tool opens arbitrary hosts; the deployment allows one.
        "web_fetch_enabled": false,
        "oauth2_issuer": ISSUER,
        "oauth2_client_id": CLIENT_ID,
        "cortex_oauth_enabled": true,
        "subscription_tier": "premium",
        "subscription_tier_display": "Cortex",
        "accept_request_encodings": ["zstd"],
        "sharing_enabled": true,
        // The official plugin source is a third-party git host; never register it.
        "official_marketplace_auto_register": false,
        // Tools and commands that need a backend llama.cpp cannot provide.
        "imagine_tools_disabled": ["image_gen", "image_edit", "image_to_video", "reference_to_video", "video_gen"],
        "workspace_command_enabled": false,
        "managed_mcps_enabled": false,
        "managed_mcp_gateway_tools_enabled": false,
    });
    if crate::search::configured(&state) {
        body["web_search_model"] = json!(state.config.model_id);
    }
    if let (Some(_), Some(model)) = (&state.config.embed_url, &state.config.embed_model) {
        body["memory_enabled"] = json!(true);
        body["memory_embedding_model"] = json!(model);
        body["memory_embedding_dimensions"] = json!(state.config.embed_dimensions);
    }
    if state.vision.load(std::sync::atomic::Ordering::Relaxed) {
        body["image_description_model"] = json!(state.config.model_id);
    }
    Json(body)
}

pub async fn user(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Query(query): Query<UserQuery>,
) -> Response {
    let Some(user) = state.db.user_by_id(user_id).await.ok().flatten() else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let mut body = json!({
        "userId": user.id.to_string(),
        "email": user.email,
        "firstName": user.first_name,
        "lastName": user.last_name,
        "codingDataRetentionOptOut": user.opt_out,
    });
    if query.include.as_deref() == Some("subscription") {
        body["subscriptionTier"] = json!("premium");
    }
    Json(body).into_response()
}

pub async fn models(State(state): State<AppState>, AuthUser(_user): AuthUser) -> impl IntoResponse {
    Json(model_list(&state))
}

pub fn model_list(state: &AppState) -> serde_json::Value {
    json!({
        "data": [{
            "id": state.config.model_id,
            "model": state.config.model_id,
            "name": state.config.model_id,
            "context_window": crate::inference::advertised_context_window(state),
            "api_backend": state.config.api_backend,
            "supports_backend_search": crate::search::configured(state),
        }]
    })
}

pub async fn subagent_bundle(AuthUser(_user): AuthUser) -> impl IntoResponse {
    Json(SubagentBundle::empty("1"))
}

pub async fn bundle_archive(AuthUser(_user): AuthUser) -> Response {
    let bytes = match empty_archive() {
        Ok(bytes) => bytes,
        Err(()) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    (
        [(axum::http::header::CONTENT_TYPE, "application/gzip")],
        bytes,
    )
        .into_response()
}

fn empty_archive() -> Result<Vec<u8>, ()> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    {
        let mut builder = tar::Builder::new(&mut gz);
        let payload = br#"{"version":"1"}"#;
        let mut header = tar::Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "bundle.json", &payload[..])
            .map_err(|_| ())?;
        builder.finish().map_err(|_| ())?;
    }
    gz.finish().map_err(|_| ())
}

pub async fn deployment_config(AuthUser(_user): AuthUser, headers: HeaderMap) -> Response {
    let mut response = Json(json!({})).into_response();
    if let Some(nonce) = headers.get("x-cortex-managed-config-nonce")
        && let Ok(name) = HeaderName::try_from("x-cortex-managed-config-nonce")
    {
        response.headers_mut().insert(name, nonce.clone());
    }
    response
}

pub async fn privacy(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Json(body): Json<PrivacyBody>,
) -> Response {
    if state
        .db
        .set_opt_out(user_id, body.coding_data_retention_opt_out)
        .await
        .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    Json(json!({
        "codingDataRetentionOptOut": body.coding_data_retention_opt_out,
    }))
    .into_response()
}

pub async fn feedback(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    bytes: Bytes,
) -> Response {
    let submission: FeedbackSubmission = match serde_json::from_slice(&bytes) {
        Ok(submission) => submission,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    if submission.session_id.is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if store_payload(&state, Some(user), "feedback", &bytes)
        .await
        .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let response = FeedbackResponse {
        feedback_id: Uuid::new_v4().to_string(),
        created_at: Utc::now(),
    };
    Json(response).into_response()
}

pub async fn feedback_complete(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    bytes: Bytes,
) -> Response {
    if serde_json::from_slice::<FeedbackSubmission>(&bytes).is_err() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if store_payload(&state, Some(user), "feedback-complete", &bytes)
        .await
        .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let _ = id;
    StatusCode::OK.into_response()
}

pub async fn feedback_dismiss(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    bytes: Bytes,
) -> Response {
    if store_payload(&state, Some(user), "feedback-dismiss", &bytes)
        .await
        .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    Json(FeedbackRequestUpdateResponse {
        request_id: id,
        status: FeedbackRequestStatus::Dismissed,
        feedback_id: None,
        updated_at: Utc::now(),
    })
    .into_response()
}

pub async fn signals(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(session_id): Path<String>,
    bytes: Bytes,
) -> Response {
    if serde_json::from_slice::<SessionSignalsUpdate>(&bytes).is_err() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if retained(&state, user).await
        && store_payload(&state, Some(user), "signals", &bytes)
            .await
            .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    Json(SessionSignalsUpdateResponse {
        session_id,
        updated_at: Utc::now(),
    })
    .into_response()
}

pub async fn traces(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    bytes: Bytes,
) -> Response {
    if retained(&state, user).await
        && store_payload(&state, Some(user), "traces", &bytes)
            .await
            .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    StatusCode::OK.into_response()
}

/// The release pointer. `<dist_dir>/stable`, written by the deploy after every artifact is in
/// place, wins so a client-only release needs no restart; the configured version is the fallback
/// when the file is missing or does not hold a version.
pub async fn channel(State(state): State<AppState>) -> impl IntoResponse {
    let published = tokio::fs::read_to_string(state.config.dist_dir.join("stable"))
        .await
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|version| {
            version.chars().next().is_some_and(|c| c.is_ascii_digit())
                && version
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        });
    format!(
        "{}\n",
        published.unwrap_or_else(|| state.config.release_version.clone())
    )
}

/// Install scripts and client builds, by exact name only: the installer and
/// `cortex update` ask for `cortex-<version>-<platform>[.exe][.zst|.gz]`.
fn dist_name_allowed(name: &str) -> bool {
    const SCRIPTS: [&str; 4] = [
        "install.sh",
        "install.ps1",
        "install-enterprise.sh",
        "install-enterprise.ps1",
    ];
    if SCRIPTS.contains(&name) {
        return true;
    }
    let Some(rest) = name.strip_prefix("cortex-") else {
        return false;
    };
    let rest = rest
        .strip_suffix(".zst")
        .or_else(|| rest.strip_suffix(".gz"))
        .unwrap_or(rest);
    let rest = rest.strip_suffix(".exe").unwrap_or(rest);
    let Some((version, platform)) = ["linux", "macos", "windows"].iter().find_map(|os| {
        let at = rest.find(&format!("-{os}-"))?;
        Some((&rest[..at], &rest[at + 1..]))
    }) else {
        return false;
    };
    let version_ok = !version.is_empty()
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        && version.chars().next().is_some_and(|c| c.is_ascii_digit());
    let platform_ok = platform
        .split_once('-')
        .map(|(_, arch)| arch)
        .is_some_and(|arch| arch == "x86_64" || arch == "aarch64");
    version_ok && platform_ok
}

pub async fn dist(
    State(state): State<AppState>,
    Path(file): Path<String>,
    request: axum::extract::Request,
) -> Response {
    use tower::ServiceExt;
    if !dist_name_allowed(&file) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match tower_http::services::ServeFile::new(state.config.dist_dir.join(&file))
        .oneshot(request)
        .await
    {
        Ok(response) => response.into_response(),
        Err(never) => match never {},
    }
}

/// The client dials the origin once per session to warm its connection pool and
/// ignores the body; an answer here keeps that probe from looking like an error.
pub async fn root() -> impl IntoResponse {
    "cortex-api\n"
}

/// The client probes its API key here before advertising it. A revoked or unknown key is a 401
/// through the extractor; a live key answers with its record. There are no teams and no per-key
/// blocking in this deployment, so a live key is never blocked or disabled.
pub async fn api_key_info(Caller { user, api_key }: Caller) -> Response {
    let Some(key) = api_key else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "this endpoint describes the API key the request was made with"})),
        )
            .into_response();
    };
    Json(json!({
        "api_key_id": key.id,
        "name": key.name,
        "redacted_api_key": format!("{API_KEY_PREFIX}…{}", key.key_suffix),
        "user_id": user,
        "create_time": key.created_at,
        "api_key_blocked": false,
        "api_key_disabled": false,
        "team_blocked": false,
    }))
    .into_response()
}

/// Usage against the daily inference allowance, in the credits shape the `/usage`
/// view reads. There is no money in this deployment: the allowance is the quota.
pub async fn billing(State(state): State<AppState>, AuthUser(user): AuthUser) -> Response {
    let used = match state.db.usage_today(user, "inference").await {
        Ok(used) => used,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let limit = state.config.daily_inference_quota.max(1);
    let percent = (f64::from(used) / f64::from(limit) * 100.0).min(100.0);
    let start = Utc::now()
        .date_naive()
        .and_time(chrono::NaiveTime::MIN)
        .and_utc();
    let end = start + chrono::Duration::days(1);
    Json(json!({
        "config": {
            "creditUsagePercent": percent,
            "currentPeriod": {
                "type": "USAGE_PERIOD_TYPE_DAILY",
                "start": start.to_rfc3339(),
                "end": end.to_rfc3339(),
            },
        },
    }))
    .into_response()
}

pub async fn auto_topup_rule(AuthUser(_user): AuthUser) -> impl IntoResponse {
    Json(json!({"rule": {"enabled": false}}))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsentBody {
    notice_id: String,
    version: i32,
}

pub async fn consent_accept(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    bytes: Bytes,
) -> Response {
    let Ok(body) = serde_json::from_slice::<ConsentBody>(&bytes) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if store_payload(&state, Some(user), "consent", &bytes)
        .await
        .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    Json(json!({"noticeId": body.notice_id, "version": body.version})).into_response()
}

/// Solicited feedback is off: the master switch is false, so the client never
/// asks for a rating. Feedback a user submits themselves is still stored.
pub async fn feedback_config(AuthUser(_user): AuthUser) -> Response {
    let config: Result<FeedbackHeuristicsConfig, _> = serde_json::from_value(json!({
        "config_id": "cortex",
        "config_version": 1,
        "enabled": false,
    }));
    match config {
        Ok(config) => Json(config).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn feedback_request_create(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    bytes: Bytes,
) -> Response {
    let Ok(input) = serde_json::from_slice::<CreateFeedbackRequestInput>(&bytes) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if retained(&state, user).await
        && store_payload(&state, Some(user), "feedback_request", &bytes)
            .await
            .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    Json(CreateFeedbackRequestResponse {
        request_id: input.request_id,
        created_at: Utc::now(),
    })
    .into_response()
}

pub async fn turn_deltas(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Path(session_id): Path<String>,
    bytes: Bytes,
) -> Response {
    let Ok(delta) = serde_json::from_slice::<SessionTurnDelta>(&bytes) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if retained(&state, user).await
        && store_payload(&state, Some(user), "turn_delta", &bytes)
            .await
            .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    Json(SessionTurnDeltaResponse {
        session_id,
        turn_number: delta.turn_number,
        recorded_at: Utc::now(),
    })
    .into_response()
}

/// Release notes by exact version name, from the changelogs folder of the dist directory.
pub async fn changelog(
    State(state): State<AppState>,
    Path(file): Path<String>,
    request: axum::extract::Request,
) -> Response {
    use tower::ServiceExt;
    let allowed = file
        .strip_suffix(".external.md")
        .or_else(|| file.strip_suffix(".external.json"))
        .is_some_and(|version| {
            !version.is_empty()
                && version.starts_with(|c: char| c.is_ascii_digit())
                && version
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        });
    if !allowed {
        return StatusCode::NOT_FOUND.into_response();
    }
    match tower_http::services::ServeFile::new(state.config.dist_dir.join("changelogs").join(&file))
        .oneshot(request)
        .await
    {
        Ok(response) => response.into_response(),
        Err(never) => match never {},
    }
}

#[derive(Deserialize)]
pub struct MixpanelForm {
    data: String,
}

/// The product-analytics protocol carries no bearer token, so this sink is
/// unauthenticated; it is held to a small body and a per-address rate.
pub async fn mixpanel(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<MixpanelForm>,
) -> Response {
    let ip = crate::client_ip(addr, &headers);
    if let Err(retry) = state.limiter.hit(
        &format!("mixpanel:{ip}"),
        60,
        std::time::Duration::from_secs(60),
    ) {
        return crate::too_many(retry);
    }
    if form.data.len() > 256 * 1024 {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(form.data.as_bytes()) else {
        return Json(json!({"error": "data", "status": 0})).into_response();
    };
    if bytes.is_empty()
        || store_payload(&state, None, "mixpanel", &bytes)
            .await
            .is_err()
    {
        return Json(json!({"error": "data", "status": 0})).into_response();
    }
    Json(json!({"error": null, "status": 1})).into_response()
}

/// Traces and signals are coding data: a user who opted out of retention has none kept.
async fn retained(state: &AppState, user: Uuid) -> bool {
    match state.db.user_by_id(user).await {
        Ok(Some(user)) => !user.opt_out,
        _ => true,
    }
}

async fn store_payload(
    state: &AppState,
    user: Option<Uuid>,
    kind: &str,
    bytes: &[u8],
) -> Result<(), DbError> {
    state.db.telemetry_store(user, kind, bytes).await
}
