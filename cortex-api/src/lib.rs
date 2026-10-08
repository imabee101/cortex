mod background;
mod control;
mod db;
mod inference;
pub mod jwt;
mod limits;
mod oidc;
mod pages;
mod product;
mod relay;
mod search;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::extract::Request;
use axum::extract::State;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::post;
use axum::routing::put;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::db::Db;
use crate::jwt::KeySet;

pub const ISSUER: &str = "https://llm.imabee.com";
pub const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
pub const TOKEN_HEADER: &str = "cortex-cli";

#[derive(Clone)]
pub struct Config {
    pub listen: SocketAddr,
    pub database_url: String,
    pub context_window: u64,
    /// Share of the context window advertised to the harness, 1 to 100.
    pub context_percent: u64,
    pub model_id: String,
    /// Wire protocol the harness uses for sampling: chat_completions, responses, or messages.
    pub api_backend: String,
    pub release_version: String,
    pub login_limit: i32,
    pub login_window_secs: i32,
    pub register_limit: i32,
    pub register_window_secs: i32,
    pub lock_after: i32,
    pub lock_secs: i32,
    pub llama_url: Option<String>,
    pub llama_api_key: Option<String>,
    pub embed_url: Option<String>,
    /// Model name and vector size the settings response advertises for memory.
    pub embed_model: Option<String>,
    pub embed_dimensions: u32,
    /// Directory holding the install scripts and client builds served under `/cli`.
    pub dist_dir: std::path::PathBuf,
    pub brave_token: Option<String>,
    pub brave_base: String,
    /// Concurrent inference requests; 0 takes `total_slots` from llama-server `/props`.
    pub parallel: usize,
    /// Requests allowed to wait for a slot before the rest get 429.
    pub queue_max: usize,
    pub queue_wait_secs: u64,
    /// Ceiling on generated tokens per request; 0 leaves requests unbounded.
    pub max_output_tokens: u64,
    pub ip_rate_per_min: u32,
    pub user_rate_per_min: u32,
    pub inference_per_min: u32,
    pub daily_inference_quota: i32,
    pub daily_search_quota: i32,
    pub telemetry_retention_days: i32,
    /// How long shutdown waits for open streams before it stops them.
    pub drain_secs: u64,
    pub key_reload_secs: u64,
    pub purge_secs: u64,
    /// Two-second waits for llama-server `/props` at start before serving with defaults.
    pub props_wait_tries: u32,
    /// How often the context window and vision flag are re-read from llama-server.
    pub props_refresh_secs: u64,
}

impl Config {
    pub fn new(listen: SocketAddr, database_url: String) -> Self {
        Self {
            listen,
            database_url,
            context_window: 98304,
            context_percent: 40,
            model_id: "Qwen3.5-9B".to_owned(),
            api_backend: "chat_completions".to_owned(),
            release_version: "0.0.0".to_owned(),
            login_limit: 10,
            login_window_secs: 900,
            register_limit: 20,
            register_window_secs: 3600,
            lock_after: 5,
            lock_secs: 900,
            llama_url: None,
            llama_api_key: None,
            embed_url: None,
            embed_model: None,
            embed_dimensions: 1024,
            dist_dir: std::path::PathBuf::from("/opt/cortex/dist"),
            brave_token: None,
            brave_base: "https://api.search.brave.com/res/v1".to_owned(),
            parallel: 0,
            queue_max: 8,
            queue_wait_secs: 30,
            max_output_tokens: 24576,
            ip_rate_per_min: 1200,
            user_rate_per_min: 1200,
            inference_per_min: 120,
            daily_inference_quota: 5000,
            daily_search_quota: 300,
            telemetry_retention_days: 30,
            drain_secs: 900,
            key_reload_secs: 60,
            purge_secs: 3600,
            props_wait_tries: 0,
            props_refresh_secs: 60,
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Db,
    pub keys: Arc<RwLock<Arc<KeySet>>>,
    pub requests: Arc<AtomicU64>,
    pub throttled: Arc<AtomicU64>,
    /// Streams cut because the model was repeating itself.
    pub loops_aborted: Arc<AtomicU64>,
    /// Streams cut because the output ceiling ended a generation that was all reasoning.
    pub reasoning_overruns: Arc<AtomicU64>,
    pub draining: Arc<AtomicBool>,
    pub limiter: Arc<limits::Limiter>,
    pub llama_breaker: Arc<limits::Breaker>,
    pub embed_breaker: Arc<limits::Breaker>,
    pub slot_context: Arc<AtomicU64>,
    pub vision: Arc<AtomicBool>,
    pub http: reqwest::Client,
    pub relay: tokio::sync::broadcast::Sender<i64>,
    pub search: Arc<search::SearchState>,
    pub slots: Arc<limits::SlotQueue>,
}

impl AppState {
    /// The signing keys in force right now; a background task swaps them on rotation.
    pub fn keys(&self) -> Arc<KeySet> {
        match self.keys.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

pub struct Running {
    pub addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    handle: tokio::task::JoinHandle<Result<(), std::io::Error>>,
    draining: Arc<AtomicBool>,
}

impl Running {
    /// Stop accepting connections and wait for open requests and streams to finish.
    pub async fn shutdown(mut self) -> Result<(), std::io::Error> {
        self.draining
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        match self.handle.await {
            Ok(result) => result,
            Err(_) => Ok(()),
        }
    }
}

pub async fn serve(config: Config) -> anyhow::Result<Running> {
    let db = Db::connect(&config.database_url)
        .await
        .map_err(|_| anyhow::anyhow!("database connection failed"))?;
    let keys = Arc::new(RwLock::new(Arc::new(
        db.load_keys()
            .await
            .map_err(|_| anyhow::anyhow!("signing key failed"))?,
    )));
    // Loopback upstreams plus one HTTPS API; the platform trust store is the policy here.
    #[allow(clippy::disallowed_methods)]
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        // llama-server closes keep-alive connections on its own clock; a request sent on
        // one it just closed fails, and a POST cannot be replayed. Nothing is pooled.
        .pool_max_idle_per_host(0)
        .tcp_user_timeout(Duration::from_secs(600))
        .build()
        .map_err(|_| anyhow::anyhow!("http client failed"))?;
    let slot_context = Arc::new(AtomicU64::new(0));
    let vision = Arc::new(AtomicBool::new(false));
    let mut parallel = config.parallel;
    if let Some(origin) = config.llama_url.clone() {
        // llama-server may still be loading its model; give it a bounded wait.
        let mut props =
            inference::fetch_props(&http, &origin, config.llama_api_key.as_deref()).await;
        for _ in 0..config.props_wait_tries {
            if props.n_ctx > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            props = inference::fetch_props(&http, &origin, config.llama_api_key.as_deref()).await;
        }
        if props.n_ctx > 0 {
            slot_context.store(props.n_ctx, std::sync::atomic::Ordering::Relaxed);
        }
        vision.store(props.vision, std::sync::atomic::Ordering::Relaxed);
        if parallel == 0 {
            parallel = props.total_slots;
        }
    }
    let parallel = parallel.max(1);
    let relay_url = config.database_url.clone();
    let (relay_tx, _) = tokio::sync::broadcast::channel(256);
    relay::spawn_listener(relay_url, relay_tx.clone());
    let slots = Arc::new(limits::SlotQueue::new(
        parallel,
        config.queue_max,
        Duration::from_secs(config.queue_wait_secs),
    ));
    let state = AppState {
        config: Arc::new(config),
        db,
        keys,
        requests: Arc::new(AtomicU64::new(0)),
        throttled: Arc::new(AtomicU64::new(0)),
        loops_aborted: Arc::new(AtomicU64::new(0)),
        reasoning_overruns: Arc::new(AtomicU64::new(0)),
        draining: Arc::new(AtomicBool::new(false)),
        limiter: Arc::new(limits::Limiter::default()),
        llama_breaker: Arc::new(limits::Breaker::new(5, 15_000)),
        embed_breaker: Arc::new(limits::Breaker::new(5, 15_000)),
        slot_context,
        vision,
        http,
        relay: relay_tx,
        search: Arc::new(search::SearchState::default()),
        slots,
    };
    background::spawn(state.clone());
    let listener = TcpListener::bind(state.config.listen).await?;
    let addr = listener.local_addr()?;
    let draining = state.draining.clone();
    let app = router(state);
    let (tx, rx) = oneshot::channel();
    let handle = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = rx.await;
        })
        .await
    });
    Ok(Running {
        addr,
        shutdown: Some(tx),
        handle,
        draining,
    })
}

pub fn router(state: AppState) -> Router {
    let control = Router::new()
        .route("/.well-known/openid-configuration", get(oidc::discovery))
        .route("/oauth2/jwks", get(oidc::jwks))
        .route("/authorize", get(oidc::authorize))
        .route("/login", post(oidc::login))
        .route("/register", get(oidc::register_form).post(oidc::register))
        .route("/consent", post(oidc::consent))
        .route("/oauth2/token", post(oidc::token))
        .route("/oauth2/device/code", post(oidc::device_code))
        .route("/device", get(oidc::device_page))
        .route("/device/decide", post(oidc::device_decide))
        .route("/healthz", get(control::healthz))
        .route("/readyz", get(control::readyz))
        .route("/metrics", get(control::metrics))
        .route("/cli/stable", get(control::channel))
        .route("/cli/alpha", get(control::channel))
        .route("/stable", get(control::channel))
        .route("/alpha", get(control::channel))
        .route("/", get(control::root))
        .route("/cli/changelogs/{file}", get(control::changelog))
        .route("/cli/{file}", get(control::dist))
        .route("/{file}", get(control::dist))
        .route("/track", post(control::mixpanel))
        .route("/engage", post(control::mixpanel))
        .route("/v1/login-config", get(control::login_config))
        .route("/v1/settings", get(control::settings))
        .route("/v1/api-key", get(control::api_key_info))
        .route("/v1/billing", get(control::billing))
        .route("/v1/auto-topup-rule", get(control::auto_topup_rule))
        .route("/v1/consent/accept", post(control::consent_accept))
        .route("/v1/feedback/config", get(control::feedback_config))
        .route(
            "/v1/feedback/requests",
            post(control::feedback_request_create),
        )
        .route("/v1/sessions/{id}/turn-deltas", post(control::turn_deltas))
        .route("/v1/tokenize-text", post(inference::tokenize_text))
        .route("/v1/user", get(control::user))
        .route("/v1/models", get(control::models))
        .route("/v1/models-v2", get(control::models))
        .route("/v1/subagents/bundle", get(control::subagent_bundle))
        .route("/v1/bundle/archive", get(control::bundle_archive))
        .route("/v1/deployment/config", get(control::deployment_config))
        .route("/v1/privacy/coding-data-retention", put(control::privacy))
        .route("/v1/feedback", post(control::feedback))
        .route(
            "/v1/feedback/requests/{id}/complete",
            post(control::feedback_complete),
        )
        .route(
            "/v1/feedback/requests/{id}/dismiss",
            post(control::feedback_dismiss),
        )
        .route("/v1/sessions/{id}/signals", post(control::signals))
        .route("/v1/traces", post(control::traces))
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024));
    let inference = Router::new()
        .route("/v1/chat/completions", post(inference::chat))
        .route("/v1/responses", post(inference::responses))
        .route("/v1/messages", post(inference::messages))
        .route("/v1/embeddings", post(inference::embeddings))
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024));
    let product = Router::new()
        .route("/sessions", get(product::list_sessions))
        .route("/sessions/{id}", put(product::upsert_session))
        .route(
            "/sessions/{id}/data",
            post(product::save_session)
                .get(product::load_session)
                .delete(product::delete_session),
        )
        .route("/sessions/{id}/share", post(product::share_session))
        .route("/build/share/{id}", get(product::read_share))
        .route(
            "/rest/app-chat/conversations",
            get(product::list_conversations),
        )
        .route(
            "/rest/app-chat/conversations/{id}",
            put(product::update_conversation),
        )
        .route(
            "/rest/app-chat/conversations/soft/{id}",
            delete(product::delete_conversation),
        )
        .route("/rest/workspaces", get(product::list_workspaces))
        .route("/rest/skills", post(product::list_skills))
        .route("/rest/user-skills", get(product::list_user_skills))
        .route("/rest/modes", post(product::list_modes))
        .route("/v1/sandbox/sessions/fork", post(product::fork_sandbox))
        .route("/v1/sandbox/sessions/{id}", delete(product::delete_sandbox))
        .route(
            "/v1/sandbox/environments",
            get(product::list_environments).post(product::create_environment),
        )
        .route(
            "/v1/sandbox/environments/{id}",
            put(product::update_environment).delete(product::delete_environment),
        )
        .route("/v1/sessions/register", post(product::register_session))
        .route("/v1/sessions/search", get(product::search_sessions))
        .route(
            "/v1/sessions/{id}/replicas/update",
            post(product::update_replica),
        )
        .route(
            "/v1/sessions/{id}/replicas/finalize",
            post(product::finalize_replica),
        )
        .route("/v1/sessions/{id}/replicas", get(product::get_replica))
        .route("/v1/sessions/{id}/download", get(product::download_replica))
        .route("/v1/storage/limits", get(product::storage_limits))
        .route("/v1/storage/exists", get(product::storage_exists))
        .route(
            "/v1/storage/batch_exists",
            post(product::storage_batch_exists),
        )
        .route(
            "/v1/storage/batch_upload",
            post(product::storage_batch_upload),
        )
        .route(
            "/v1/storage/batch_upload_json",
            post(product::storage_batch_upload_json),
        )
        .route("/v1/storage/download", get(product::storage_download))
        .route("/v1/storage", post(product::storage_upload))
        .route("/v1/storage/multipart/init", post(product::multipart_init))
        .route(
            "/v1/storage/multipart/{id}/complete",
            post(product::multipart_complete),
        )
        .route(
            "/v1/storage/signed-upload-url",
            post(product::signed_upload),
        )
        .route("/v1/storage/put/{token}", put(product::storage_put))
        .route("/v1/storage/get/{token}", get(product::storage_get))
        .route("/v1/storage/part/{token}", put(product::storage_part))
        .route("/ws/code-agent", get(relay::code_agent))
        .route("/ws/gw", get(relay::gateway))
        .route("/ws/gw/", get(relay::gateway))
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024));
    Router::new()
        .merge(control)
        .merge(inference)
        .merge(product)
        .layer(axum::middleware::from_fn(timeout_mw))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            ip_limit_mw,
        ))
        .layer(axum::middleware::from_fn_with_state(state.clone(), log_mw))
        .with_state(state)
}

async fn timeout_mw(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_owned();
    if long_lived(&path) {
        return next.run(req).await;
    }
    match tokio::time::timeout(Duration::from_secs(60), next.run(req)).await {
        Ok(response) => response,
        Err(_) => StatusCode::GATEWAY_TIMEOUT.into_response(),
    }
}

fn long_lived(path: &str) -> bool {
    matches!(
        path,
        "/v1/chat/completions" | "/v1/responses" | "/v1/messages"
    ) || path.starts_with("/ws/")
        || path.starts_with("/v1/stt")
}

async fn log_mw(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let started = std::time::Instant::now();
    let mut response = next.run(req).await;
    if let Ok(value) = axum::http::HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", value);
    }
    state
        .requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    tracing::info!(
        %method,
        %path,
        status = %response.status(),
        %request_id,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "request"
    );
    response
}

/// The caller's address. Behind the loopback edge the peer is always the proxy,
/// so the address it forwards is used instead; from any other peer the header is ignored.
/// The error chain of a failed upstream call, for logs. It carries socket and protocol
/// causes only, never request content.
pub fn upstream_cause(err: &reqwest::Error) -> String {
    let mut parts = vec![err.to_string()];
    let mut source = std::error::Error::source(err);
    while let Some(next) = source {
        parts.push(next.to_string());
        source = next.source();
    }
    parts.join(": ")
}

pub fn client_ip(addr: SocketAddr, headers: &axum::http::HeaderMap) -> String {
    if addr.ip().is_loopback()
        && let Some(forwarded) = headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next())
            .map(str::trim)
            .filter(|value| value.parse::<std::net::IpAddr>().is_ok())
    {
        return forwarded.to_owned();
    }
    addr.ip().to_string()
}

pub fn too_many(retry_after: u64) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(axum::http::header::RETRY_AFTER, retry_after.to_string())],
        axum::Json(
            serde_json::json!({"error": {"message": "rate limit exceeded", "type": "rate_limit"}}),
        ),
    )
        .into_response()
}

async fn ip_limit_mw(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    if matches!(path, "/healthz" | "/readyz" | "/metrics") {
        return next.run(req).await;
    }
    let ip = client_ip(addr, req.headers());
    if let Err(retry) = state.limiter.hit(
        &format!("ip:{ip}"),
        state.config.ip_rate_per_min,
        Duration::from_secs(60),
    ) {
        state
            .throttled
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return too_many(retry);
    }
    next.run(req).await
}

pub fn oauth_error(status: StatusCode, error: &str, retry_after: i32) -> Response {
    let body = serde_json::json!({ "error": error });
    if retry_after > 0 {
        return (
            status,
            [(axum::http::header::RETRY_AFTER, retry_after.to_string())],
            axum::Json(body),
        )
            .into_response();
    }
    (status, axum::Json(body)).into_response()
}

mod auth {
    use super::*;
    use axum::extract::FromRequestParts;
    use axum::http::request::Parts;
    use uuid::Uuid;

    pub struct AuthUser(pub Uuid);

    impl FromRequestParts<AppState> for AuthUser {
        type Rejection = Response;

        async fn from_request_parts(
            parts: &mut Parts,
            state: &AppState,
        ) -> Result<Self, Self::Rejection> {
            user_from_headers(state, &parts.headers).map(AuthUser)
        }
    }

    pub(crate) fn user_from_headers(
        state: &AppState,
        headers: &axum::http::HeaderMap,
    ) -> Result<Uuid, Response> {
        if let Some(header) = headers.get("x-cortex-token-auth") {
            let Ok(value) = header.to_str() else {
                return Err(StatusCode::UNAUTHORIZED.into_response());
            };
            if value != TOKEN_HEADER {
                return Err(StatusCode::UNAUTHORIZED.into_response());
            }
        }
        let Some(auth) = headers.get(axum::http::header::AUTHORIZATION) else {
            return Err(StatusCode::UNAUTHORIZED.into_response());
        };
        let Ok(auth) = auth.to_str() else {
            return Err(StatusCode::UNAUTHORIZED.into_response());
        };
        let Some(token) = auth.strip_prefix("Bearer ") else {
            return Err(StatusCode::UNAUTHORIZED.into_response());
        };
        let claims = jwt::verify_access(token, &state.keys().published)
            .map_err(|_| StatusCode::UNAUTHORIZED.into_response())?;
        state
            .limiter
            .hit(
                &format!("user:{}", claims.sub),
                state.config.user_rate_per_min,
                Duration::from_secs(60),
            )
            .map_err(|retry| {
                state
                    .throttled
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                too_many(retry)
            })?;
        Ok(claims.sub)
    }
}
