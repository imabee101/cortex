use std::sync::atomic::Ordering;

use axum::body::Body;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header::CONTENT_ENCODING;
use axum::http::header::CONTENT_TYPE;
use axum::http::header::RETRY_AFTER;
use axum::response::IntoResponse;
use axum::response::Response;
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use tokio::sync::OwnedSemaphorePermit;

use crate::AppState;
use crate::auth::AuthUser;

const CONTENT_INDEX_EVENTS: &[&str] = &[
    "response.content_part.added",
    "response.content_part.done",
    "response.output_text.delta",
    "response.output_text.done",
    "response.output_text.annotation.added",
    "response.refusal.delta",
    "response.refusal.done",
    "response.reasoning_text.delta",
    "response.reasoning_text.done",
];

pub async fn chat(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    forward(&state, user, "/v1/chat/completions", headers, body).await
}

pub async fn responses(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match crate::search::maybe(&state, user, &headers, &body).await {
        crate::search::Outcome::Answer(response) => response,
        crate::search::Outcome::Rewritten(body) => {
            let mut headers = headers;
            headers.remove(CONTENT_ENCODING);
            forward(&state, user, "/v1/responses", headers, body).await
        }
        crate::search::Outcome::Pass => forward(&state, user, "/v1/responses", headers, body).await,
    }
}

/// Token count for a piece of text, from the pinned model's own tokenizer.
pub async fn tokenize_text(
    State(state): State<AppState>,
    AuthUser(_user): AuthUser,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(origin) = state.config.llama_url.as_deref() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "inference upstream is not configured",
        );
    };
    let Ok(bytes) = decode_body(&headers, body) else {
        return error_response(StatusCode::BAD_REQUEST, "request body could not be decoded");
    };
    let Some(text) = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|payload| {
            payload
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
    else {
        return error_response(StatusCode::BAD_REQUEST, "request needs a text field");
    };
    if let Err(retry) = state.llama_breaker.check() {
        return unavailable(retry);
    }
    let mut request = state
        .http
        .post(format!("{}/tokenize", origin.trim_end_matches('/')))
        .json(&json!({"content": text}));
    if let Some(key) = state
        .config
        .llama_api_key
        .as_deref()
        .filter(|key| !key.is_empty())
    {
        request = request.bearer_auth(key);
    }
    let upstream = match request.send().await {
        Ok(upstream) => upstream,
        Err(err) => {
            tracing::warn!(
                upstream = "tokenize",
                cause = %crate::upstream_cause(&err),
                "tokenize upstream request failed"
            );
            state.llama_breaker.failure();
            return error_response(StatusCode::BAD_GATEWAY, "inference upstream is unavailable");
        }
    };
    if !upstream.status().is_success() {
        return error_response(StatusCode::BAD_GATEWAY, "tokenizer rejected the text");
    }
    let tokens = upstream
        .json::<Value>()
        .await
        .ok()
        .and_then(|reply| reply.get("tokens").cloned());
    match tokens {
        Some(tokens) if tokens.is_array() => {
            axum::Json(json!({"token_ids": tokens})).into_response()
        }
        _ => error_response(
            StatusCode::BAD_GATEWAY,
            "tokenizer reply was not understood",
        ),
    }
}

pub async fn embeddings(
    State(state): State<AppState>,
    AuthUser(_user): AuthUser,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let origin = state
        .config
        .embed_url
        .clone()
        .or_else(|| state.config.llama_url.clone());
    let Some(origin) = origin else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "embedding upstream is not configured",
        );
    };
    let bytes = match decode_body(&headers, body) {
        Ok(bytes) => bytes,
        Err(()) => {
            tracing::warn!(
                encoding = ?headers.get(CONTENT_ENCODING),
                "request body could not be decoded"
            );
            return error_response(StatusCode::BAD_REQUEST, "request body could not be decoded");
        }
    };
    let payload: Value = match serde_json::from_slice(&bytes) {
        Ok(payload) => payload,
        Err(err) => {
            tracing::warn!(
                bytes = bytes.len(),
                line = err.line(),
                column = err.column(),
                "request body is not JSON"
            );
            return error_response(StatusCode::BAD_REQUEST, "request body is not JSON");
        }
    };
    if let Err(retry) = state.embed_breaker.check() {
        return unavailable(retry);
    }
    // A separate embeddings server has its own slots; only a shared one queues behind generation.
    let _permit = if state.config.embed_url.is_some() {
        None
    } else {
        match acquire_slot(&state).await {
            Ok(permit) => Some(permit),
            Err(response) => return response,
        }
    };
    let url = format!("{}/v1/embeddings", origin.trim_end_matches('/'));
    let mut request = state.http.post(url).json(&payload);
    if let Some(key) = state
        .config
        .llama_api_key
        .as_deref()
        .filter(|key| !key.is_empty())
    {
        request = request.bearer_auth(key);
    }
    let upstream = match request.send().await {
        Ok(upstream) => upstream,
        Err(err) => {
            tracing::warn!(
                upstream = "embeddings",
                timeout = err.is_timeout(),
                connect = err.is_connect(),
                cause = %crate::upstream_cause(&err),
                "embedding upstream request failed"
            );
            state.embed_breaker.failure();
            return error_response(StatusCode::BAD_GATEWAY, "embedding upstream is unavailable");
        }
    };
    let status = upstream.status();
    if status.is_server_error() {
        state.embed_breaker.failure();
    } else {
        state.embed_breaker.success();
    }
    let retry_after = upstream
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    if !status.is_success() {
        let bytes = upstream.bytes().await.unwrap_or_default();
        return passthrough(status, "application/json", retry_after.as_deref(), bytes);
    }
    let mut response: Value = match upstream.json().await {
        Ok(response) => response,
        Err(_) => {
            return error_response(StatusCode::BAD_GATEWAY, "embedding upstream is unavailable");
        }
    };
    if let Some(dimensions) = payload.get("dimensions").and_then(|value| value.as_u64())
        && let Some(data) = response
            .get_mut("data")
            .and_then(|value| value.as_array_mut())
    {
        for item in data {
            if let Some(vector) = item
                .get_mut("embedding")
                .and_then(|value| value.as_array_mut())
                && vector.len() > dimensions as usize
            {
                vector.truncate(dimensions as usize);
            }
        }
    }
    (
        StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
        axum::Json(response),
    )
        .into_response()
}

pub async fn messages(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    forward(&state, user, "/v1/messages", headers, body).await
}

pub struct UpstreamProps {
    pub n_ctx: u64,
    pub vision: bool,
    pub total_slots: usize,
}

pub async fn fetch_props(
    client: &reqwest::Client,
    origin: &str,
    api_key: Option<&str>,
) -> UpstreamProps {
    let url = format!("{}/props", origin.trim_end_matches('/'));
    let mut request = client.get(url);
    if let Some(key) = api_key.filter(|key| !key.is_empty()) {
        request = request.bearer_auth(key);
    }
    let Ok(response) = request.send().await else {
        return UpstreamProps {
            n_ctx: 0,
            vision: false,
            total_slots: 1,
        };
    };
    if !response.status().is_success() {
        return UpstreamProps {
            n_ctx: 0,
            vision: false,
            total_slots: 1,
        };
    }
    let Ok(body) = response.json::<Value>().await else {
        return UpstreamProps {
            n_ctx: 0,
            vision: false,
            total_slots: 1,
        };
    };
    let n_ctx = body
        .get("default_generation_settings")
        .and_then(|settings| settings.get("n_ctx"))
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let vision = body
        .pointer("/modalities/vision")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let total_slots = body
        .get("total_slots")
        .and_then(|value| value.as_u64())
        .map_or(1, |slots| slots.max(1) as usize);
    UpstreamProps {
        n_ctx,
        vision,
        total_slots,
    }
}

pub fn context_window(state: &AppState) -> u64 {
    let live = state.slot_context.load(Ordering::Relaxed);
    if live == 0 {
        state.config.context_window
    } else {
        live
    }
}

/// The window the harness is told about. It counts bytes/4 against this number and never reads the
/// server's token count, which runs about twice that on code, so the real window is scaled down.
pub fn advertised_context_window(state: &AppState) -> u64 {
    (context_window(state).saturating_mul(state.config.context_percent) / 100).max(1)
}

/// The model's chat template accepts a system message only first, but the harness
/// injects reminders mid-conversation as system or developer messages and the template
/// then fails the whole request with a 500. A late one keeps its place as a user message.
/// A responses request with top-level `instructions` already has its system message first.
pub(crate) fn demote_late_system(payload: &mut Value) {
    let instructions = payload
        .get("instructions")
        .and_then(Value::as_str)
        .is_some_and(|text| !text.is_empty());
    for (key, lead_taken) in [("messages", false), ("input", instructions)] {
        let Some(items) = payload.get_mut(key).and_then(Value::as_array_mut) else {
            continue;
        };
        for (index, item) in items.iter_mut().enumerate() {
            let late = index > 0 || lead_taken;
            let system = matches!(
                item.get("role").and_then(Value::as_str),
                Some("system" | "developer")
            );
            if late && system {
                item["role"] = json!("user");
            }
        }
    }
}

/// A generation that never stops (a model stuck repeating itself, a request with no limit)
/// holds the only slot while everyone else queues and then fails. Every request gets a ceiling:
/// the client's own limit when it is lower, otherwise `cap`. Zero turns the ceiling off.
pub(crate) fn cap_output(payload: &mut Value, default_key: &str, cap: u64) {
    if cap == 0 {
        return;
    }
    let Some(object) = payload.as_object_mut() else {
        return;
    };
    let mut present = false;
    for key in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
        if let Some(value) = object.get_mut(key) {
            present = true;
            if value.as_u64().is_none_or(|asked| asked > cap) {
                *value = json!(cap);
            }
        }
    }
    if !present {
        object.insert(default_key.to_owned(), json!(cap));
    }
}

/// True when the ceiling `cap_output` applied is what limits the request, not a lower limit the
/// client asked for.
pub(crate) fn cap_binds(payload: &Value, cap: u64) -> bool {
    let limits: Vec<u64> = ["max_tokens", "max_completion_tokens", "max_output_tokens"]
        .iter()
        .filter_map(|key| payload.get(*key).and_then(Value::as_u64))
        .collect();
    cap != 0 && !limits.is_empty() && limits.iter().all(|limit| *limit == cap)
}

async fn forward(
    state: &AppState,
    user: uuid::Uuid,
    path: &str,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(origin) = state.config.llama_url.as_deref() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "inference upstream is not configured",
        );
    };
    let bytes = match decode_body(&headers, body) {
        Ok(bytes) => bytes,
        Err(()) => {
            tracing::warn!(
                encoding = ?headers.get(CONTENT_ENCODING),
                "request body could not be decoded"
            );
            return error_response(StatusCode::BAD_REQUEST, "request body could not be decoded");
        }
    };
    let mut payload: Value = match serde_json::from_slice(&bytes) {
        Ok(payload) => payload,
        Err(err) => {
            tracing::warn!(
                bytes = bytes.len(),
                line = err.line(),
                column = err.column(),
                "request body is not JSON"
            );
            return error_response(StatusCode::BAD_REQUEST, "request body is not JSON");
        }
    };
    // The override header names a model the harness picked; only one model is served.
    if headers.get("x-cortex-model-override").is_some()
        && let Some(object) = payload.as_object_mut()
    {
        object.insert("model".to_owned(), json!(state.config.model_id));
    }
    demote_late_system(&mut payload);
    cap_output(
        &mut payload,
        if path.ends_with("/responses") {
            "max_output_tokens"
        } else {
            "max_tokens"
        },
        state.config.max_output_tokens,
    );
    let cap_binds = cap_binds(&payload, state.config.max_output_tokens);
    let stream = payload
        .get("stream")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if let Err(response) = admit(state, user).await {
        return response;
    }
    let permit = match acquire_slot(state).await {
        Ok(permit) => permit,
        Err(response) => return response,
    };
    let url = format!(
        "{}/{}",
        origin.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    let mut request = state.http.post(url).json(&payload);
    if let Some(key) = state
        .config
        .llama_api_key
        .as_deref()
        .filter(|key| !key.is_empty())
    {
        request = request.bearer_auth(key);
    }
    let upstream = match request.send().await {
        Ok(upstream) => upstream,
        Err(err) => {
            tracing::warn!(
                upstream = "llama",
                timeout = err.is_timeout(),
                connect = err.is_connect(),
                cause = %crate::upstream_cause(&err),
                "inference upstream request failed"
            );
            state.llama_breaker.failure();
            return error_response(StatusCode::BAD_GATEWAY, "inference upstream is unavailable");
        }
    };
    let status = upstream.status();
    if status.is_server_error() {
        state.llama_breaker.failure();
    } else {
        state.llama_breaker.success();
    }
    let retry_after = upstream
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let content_type = upstream
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or(if stream {
            "text/event-stream"
        } else {
            "application/json"
        })
        .to_owned();
    if !status.is_success() {
        let bytes = upstream.bytes().await.unwrap_or_default();
        return passthrough(status, &content_type, retry_after.as_deref(), bytes);
    }
    if stream || content_type.contains("text/event-stream") {
        let stream = upstream.bytes_stream();
        let body = Body::from_stream(rewrite_stream(
            stream,
            permit,
            state.loops_aborted.clone(),
            state.reasoning_overruns.clone(),
            cap_binds,
        ));
        return sse_response(status, retry_after.as_deref(), body);
    }
    let bytes = match upstream.bytes().await {
        Ok(bytes) => bytes,
        Err(_) => {
            return error_response(StatusCode::BAD_GATEWAY, "inference upstream is unavailable");
        }
    };
    let rewritten = rewrite_json_bytes(&bytes).unwrap_or_else(|| bytes.to_vec());
    passthrough(
        status,
        "application/json",
        retry_after.as_deref(),
        rewritten,
    )
}

pub(crate) fn decode_body(headers: &HeaderMap, body: Bytes) -> Result<Vec<u8>, ()> {
    let encoding = headers
        .get(CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok());
    if encoding == Some("zstd") {
        zstd::stream::decode_all(body.as_ref()).map_err(|_| ())
    } else if encoding.is_some() {
        Err(())
    } else {
        Ok(body.to_vec())
    }
}

fn passthrough(
    status: reqwest::StatusCode,
    content_type: &str,
    retry_after: Option<&str>,
    bytes: impl Into<Bytes>,
) -> Response {
    let mut response = Response::builder()
        .status(status.as_u16())
        .header(CONTENT_TYPE, content_type)
        .body(Body::from(bytes.into()))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response());
    if let Some(retry_after) = retry_after
        && let Ok(value) = axum::http::HeaderValue::from_str(retry_after)
    {
        response.headers_mut().insert(RETRY_AFTER, value);
    }
    response
}

fn sse_response(status: reqwest::StatusCode, retry_after: Option<&str>, body: Body) -> Response {
    let mut response = Response::builder()
        .status(status.as_u16())
        .header(CONTENT_TYPE, "text/event-stream")
        .header(axum::http::header::CACHE_CONTROL, "no-cache")
        .body(body)
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response());
    if let Some(retry_after) = retry_after
        && let Ok(value) = axum::http::HeaderValue::from_str(retry_after)
    {
        response.headers_mut().insert(RETRY_AFTER, value);
    }
    response
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (
        status,
        axum::Json(json!({"error": {"message": message, "type": "server_error"}})),
    )
        .into_response()
}

/// Per-user rate and daily quota, and the upstream breaker, before a slot is taken.
pub(crate) async fn admit(state: &AppState, user: uuid::Uuid) -> Result<(), Response> {
    if let Err(retry) = state.llama_breaker.check() {
        return Err(unavailable(retry));
    }
    if let Err(retry) = state.limiter.hit(
        &format!("inference:{user}"),
        state.config.inference_per_min,
        std::time::Duration::from_secs(60),
    ) {
        state.throttled.fetch_add(1, Ordering::Relaxed);
        return Err(crate::too_many(retry));
    }
    match state
        .db
        .usage_hit(user, "inference", state.config.daily_inference_quota)
        .await
    {
        Ok(rate) if rate.allowed => Ok(()),
        Ok(rate) => Err(crate::too_many(rate.retry_after as u64)),
        Err(_) => Err(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "quota check failed",
        )),
    }
}

pub(crate) async fn acquire_slot(state: &AppState) -> Result<OwnedSemaphorePermit, Response> {
    state.slots.acquire().await.map_err(|_| busy_response())
}

fn unavailable(retry_after: u64) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(RETRY_AFTER, retry_after.to_string())],
        axum::Json(json!({"error": {"message": "inference upstream is unavailable", "type": "server_error"}})),
    )
        .into_response()
}

fn busy_response() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(RETRY_AFTER, "2")],
        axum::Json(json!({"error": {"message": "inference is busy", "type": "rate_limit"}})),
    )
        .into_response()
}

fn rewrite_stream(
    incoming: impl futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    permit: OwnedSemaphorePermit,
    loops_aborted: std::sync::Arc<std::sync::atomic::AtomicU64>,
    reasoning_overruns: std::sync::Arc<std::sync::atomic::AtomicU64>,
    cap_binds: bool,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send {
    futures_util::stream::unfold(
        StreamState {
            incoming: Box::pin(incoming),
            fixer: SseFix::default(),
            guard: LoopGuard::default(),
            loops_aborted,
            reasoning_overruns,
            cap_binds,
            finished: false,
            _permit: permit,
        },
        |mut state| async move {
            if state.finished {
                return None;
            }
            loop {
                match state.incoming.next().await {
                    Some(Ok(bytes)) => {
                        if state.guard.feed(&bytes) {
                            // Dropping the upstream frees the slot; the client sees a broken
                            // stream, which it retries with a fresh sample.
                            state.finished = true;
                            state.loops_aborted.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!("model output is repeating itself; stream cut");
                            return Some((
                                Err(std::io::Error::other("generation was repeating itself")),
                                state,
                            ));
                        }
                        if state.cap_binds && state.guard.overran() {
                            // The ceiling cut a generation that never left its reasoning. The
                            // harness does not retry a length finish but does retry a broken
                            // stream, and a fresh sample usually answers.
                            state.finished = true;
                            state.reasoning_overruns.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!(
                                "generation spent its whole output ceiling reasoning; stream cut"
                            );
                            return Some((
                                Err(std::io::Error::other("generation never left its reasoning")),
                                state,
                            ));
                        }
                        let out = state.fixer.push(&bytes);
                        if !out.is_empty() {
                            return Some((Ok(Bytes::from(out)), state));
                        }
                    }
                    Some(Err(_)) => {
                        state.finished = true;
                        return Some((Err(std::io::Error::other("upstream stream failed")), state));
                    }
                    None => {
                        state.finished = true;
                        let tail = state.fixer.finish();
                        if tail.is_empty() {
                            return None;
                        }
                        return Some((Ok(Bytes::from(tail)), state));
                    }
                }
            }
        },
    )
}

struct StreamState {
    incoming:
        std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    fixer: SseFix,
    guard: LoopGuard,
    loops_aborted: std::sync::Arc<std::sync::atomic::AtomicU64>,
    reasoning_overruns: std::sync::Arc<std::sync::atomic::AtomicU64>,
    cap_binds: bool,
    finished: bool,
    _permit: OwnedSemaphorePermit,
}

/// Watches the text a model streams (answer and reasoning, never tool-call arguments) for
/// output that has stopped moving: the last `WINDOW` bytes are almost all repeats of a few
/// short pieces. Normal prose and code score near 1.0 distinct pieces; a loop scores near 0.
#[derive(Default)]
struct LoopGuard {
    lines: Vec<u8>,
    text: Vec<u8>,
    unchecked: usize,
    /// An answer or a tool call has started.
    answered: bool,
    /// The upstream finished because it ran out of tokens.
    length: bool,
}

impl LoopGuard {
    const WINDOW: usize = 8192;
    const EVERY: usize = 1024;
    const PIECE: usize = 48;
    const STRIDE: usize = 1;
    /// Fewer than this share of the sampled pieces being distinct counts as a loop.
    const DISTINCT_BELOW: f64 = 0.10;

    /// True when the generation ended at the token limit having produced reasoning only.
    fn overran(&self) -> bool {
        self.length && !self.answered
    }

    /// Record whether a line carries an answer or tool call, or a finish at the token limit,
    /// in any of the three protocols.
    fn note(&mut self, line: &[u8]) {
        let Some(data) = trim_ascii_end(line).strip_prefix(b"data:") else {
            return;
        };
        let Ok(value) = serde_json::from_slice::<Value>(trim_ascii_start(data)) else {
            return;
        };
        let text = |pointer: &str| {
            value
                .pointer(pointer)
                .and_then(Value::as_str)
                .is_some_and(|text| !text.trim().is_empty())
        };
        let has = |pointer: &str| value.pointer(pointer).is_some_and(|found| !found.is_null());
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        if text("/choices/0/delta/content")
            || has("/choices/0/delta/tool_calls")
            || text("/delta/text")
            || has("/delta/partial_json")
            || value.pointer("/content_block/type").and_then(Value::as_str) == Some("tool_use")
            || (kind.ends_with("output_text.delta") && text("/delta"))
            || kind == "response.function_call_arguments.delta"
            || value.pointer("/item/type").and_then(Value::as_str) == Some("function_call")
        {
            self.answered = true;
        }
        if value
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
            == Some("length")
            || value.pointer("/delta/stop_reason").and_then(Value::as_str) == Some("max_tokens")
            || kind == "response.incomplete"
        {
            self.length = true;
        }
    }

    /// Feed raw SSE bytes; true when the model is looping.
    fn feed(&mut self, bytes: &[u8]) -> bool {
        self.lines.extend_from_slice(bytes);
        while let Some(pos) = self.lines.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.lines.drain(..=pos).collect();
            self.note(&line);
            if let Some(delta) = visible_delta(&line) {
                self.text.extend_from_slice(delta.as_bytes());
                self.unchecked += delta.len();
            }
        }
        if self.text.len() > Self::WINDOW {
            let excess = self.text.len() - Self::WINDOW;
            self.text.drain(..excess);
        }
        if self.text.len() < Self::WINDOW || self.unchecked < Self::EVERY {
            return false;
        }
        self.unchecked = 0;
        let mut seen = std::collections::HashSet::new();
        let mut total = 0usize;
        let mut at = 0;
        while at + Self::PIECE <= self.text.len() {
            let mut hash = 0xcbf29ce484222325_u64;
            for byte in &self.text[at..at + Self::PIECE] {
                hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
            }
            seen.insert(hash);
            total += 1;
            at += Self::STRIDE;
        }
        (seen.len() as f64) < Self::DISTINCT_BELOW * total as f64
    }
}

/// The answer or reasoning text carried by one SSE line, whichever protocol it is in.
fn visible_delta(line: &[u8]) -> Option<String> {
    let data = trim_ascii_start(trim_ascii_end(line).strip_prefix(b"data:")?);
    let value: Value = serde_json::from_slice(data).ok()?;
    let text = |pointer: &str| {
        value
            .pointer(pointer)
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    if value.get("choices").is_some() {
        return text("/choices/0/delta/content")
            .or_else(|| text("/choices/0/delta/reasoning_content"));
    }
    match value.get("type").and_then(Value::as_str)? {
        kind if kind.ends_with("output_text.delta")
            || kind.ends_with("reasoning_text.delta")
            || kind.ends_with("reasoning_summary_text.delta") =>
        {
            text("/delta")
        }
        "content_block_delta" => text("/delta/text").or_else(|| text("/delta/thinking")),
        _ => None,
    }
}

#[derive(Default)]
struct SseFix {
    pending: Vec<u8>,
}

impl SseFix {
    fn push(&mut self, input: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(input);
        let mut out = Vec::new();
        while let Some(pos) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=pos).collect();
            out.extend(rewrite_sse_line(&line));
        }
        out
    }

    fn finish(&mut self) -> Vec<u8> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        let line = std::mem::take(&mut self.pending);
        let mut out = rewrite_sse_line(&line);
        if !out.ends_with(b"\n") {
            out.push(b'\n');
        }
        out
    }
}

fn rewrite_sse_line(line: &[u8]) -> Vec<u8> {
    let trimmed = trim_ascii_end(line);
    let Some(data) = trimmed.strip_prefix(b"data:") else {
        return line.to_vec();
    };
    let data = trim_ascii_start(data);
    if data == b"[DONE]" {
        return line.to_vec();
    }
    let Some(rewritten) = rewrite_json_bytes(data) else {
        return line.to_vec();
    };
    let mut out = b"data: ".to_vec();
    out.extend(rewritten);
    if line.ends_with(b"\n") {
        out.push(b'\n');
    }
    out
}

fn rewrite_json_bytes(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut value: Value = serde_json::from_slice(bytes).ok()?;
    normalize_payload(&mut value);
    serde_json::to_vec(&value).ok()
}

pub fn normalize_payload(value: &mut Value) {
    match value {
        Value::Array(items) => {
            for item in items {
                normalize_payload(item);
            }
        }
        Value::Object(map) => normalize_object(map),
        _ => {}
    }
}

fn normalize_object(map: &mut Map<String, Value>) {
    if let Some(calls) = map
        .get_mut("tool_calls")
        .and_then(|calls| calls.as_array_mut())
    {
        for call in calls.iter_mut() {
            if let Some(function) = call.get_mut("function") {
                stringify_arguments(function);
            }
        }
    }
    if map.get("type").and_then(|kind| kind.as_str()) == Some("function_call") {
        stringify_arguments_map(map);
    }
    if let Some(function) = map.get_mut("function") {
        stringify_arguments(function);
    }
    if let Some(kind) = map.get("type").and_then(|kind| kind.as_str())
        && CONTENT_INDEX_EVENTS.contains(&kind)
        && !map.contains_key("content_index")
    {
        map.insert("content_index".to_owned(), json!(0));
    }
    if map.get("object").and_then(|kind| kind.as_str()) == Some("response") {
        map.entry("created_at").or_insert(json!(0));
        map.entry("model").or_insert(json!(""));
        map.entry("output").or_insert(json!([]));
        map.entry("id").or_insert(json!("resp"));
        map.entry("status").or_insert(json!("completed"));
    }
    if map.contains_key("input_tokens") || map.contains_key("output_tokens") {
        map.entry("input_tokens").or_insert(json!(0));
        map.entry("output_tokens").or_insert(json!(0));
        map.entry("total_tokens").or_insert(json!(0));
        map.entry("input_tokens_details")
            .or_insert(json!({"cached_tokens": 0}));
        map.entry("output_tokens_details")
            .or_insert(json!({"reasoning_tokens": 0}));
    }
    if map.get("type").and_then(|kind| kind.as_str()) == Some("output_text") {
        map.entry("annotations").or_insert(json!([]));
    }
    if map.get("type").and_then(|kind| kind.as_str()) == Some("thinking") {
        map.entry("signature").or_insert(json!(""));
        map.entry("thinking").or_insert(json!(""));
    }
    if map.get("type").and_then(|kind| kind.as_str()) == Some("tool_use") {
        map.entry("input").or_insert(json!({}));
        map.entry("id").or_insert(json!("tool"));
        map.entry("name").or_insert(json!("tool"));
    }
    if let Some(kind) = map
        .get("type")
        .and_then(|kind| kind.as_str())
        .map(str::to_owned)
    {
        if kind.starts_with("response.") && !map.contains_key("sequence_number") {
            map.insert("sequence_number".to_owned(), json!(1));
        }
        if kind.starts_with("response.")
            && !matches!(
                kind.as_str(),
                "response.created"
                    | "response.in_progress"
                    | "response.completed"
                    | "response.incomplete"
                    | "response.failed"
                    | "response.queued"
            )
            && !map.contains_key("output_index")
        {
            map.insert("output_index".to_owned(), json!(0));
        }
    }
    for child in map.values_mut() {
        normalize_payload(child);
    }
}

fn stringify_arguments(value: &mut Value) {
    if let Some(map) = value.as_object_mut() {
        stringify_arguments_map(map);
    }
}

fn stringify_arguments_map(map: &mut Map<String, Value>) {
    let Some(arguments) = map.get("arguments") else {
        return;
    };
    if (arguments.is_object() || arguments.is_array())
        && let Ok(text) = serde_json::to_string(arguments)
    {
        map.insert("arguments".to_owned(), Value::String(text));
    }
}

fn trim_ascii_start(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    &bytes[start..]
}

fn trim_ascii_end(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map(|pos| pos + 1)
        .unwrap_or(0);
    &bytes[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_arguments_become_a_json_string() {
        let mut payload = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "function": {"name": "shell", "arguments": {"cmd": "ls"}}
                    }]
                }
            }]
        });
        normalize_payload(&mut payload);
        let arguments = &payload["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"];
        assert_eq!(arguments, &json!("{\"cmd\":\"ls\"}"));
    }

    #[test]
    fn reasoning_delta_gains_content_index() {
        let mut event = json!({
            "type": "response.reasoning_text.delta",
            "delta": "think",
            "item_id": "rs_1",
            "output_index": 0,
            "sequence_number": 1
        });
        normalize_payload(&mut event);
        assert_eq!(event["content_index"], 0);
        assert_eq!(event["delta"], "think");
    }

    #[test]
    fn sse_done_line_is_unchanged() {
        let mut fixer = SseFix::default();
        let out = fixer.push(b"data: [DONE]\n");
        assert_eq!(out, b"data: [DONE]\n");
    }

    #[test]
    fn llama_completed_event_gains_the_fields_the_client_requires() {
        let mut event = json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "object": "response",
                "created_at": 1,
                "status": "completed",
                "model": "Qwen3.5-9B",
                "output": [],
                "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4, "input_tokens_details": {"cached_tokens": 0}}
            }
        });
        normalize_payload(&mut event);
        assert_eq!(event["sequence_number"], 1);
        assert_eq!(
            event["response"]["usage"]["output_tokens_details"]["reasoning_tokens"],
            0
        );
    }

    #[test]
    fn thinking_block_gains_an_empty_signature() {
        let mut event = json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "thinking", "thinking": ""}
        });
        normalize_payload(&mut event);
        assert_eq!(event["content_block"]["signature"], "");
    }

    #[test]
    fn tool_use_block_gains_an_input_object() {
        let mut event = json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": {"type": "tool_use", "id": "call_1", "name": "write"}
        });
        normalize_payload(&mut event);
        assert_eq!(event["content_block"]["input"], json!({}));
    }

    fn chat_line(field: &str, text: &str) -> Vec<u8> {
        format!(
            "data: {}\n\n",
            json!({"choices": [{"index": 0, "delta": {field: text}}]})
        )
        .into_bytes()
    }

    #[test]
    fn a_repeating_generation_is_cut_and_ordinary_text_is_not() {
        let mut guard = LoopGuard::default();
        let paragraph = "Let me check the file again to make sure the output is right. ";
        let mut cut = false;
        for _ in 0..400 {
            cut |= guard.feed(&chat_line("reasoning_content", paragraph));
        }
        assert!(cut, "a loop of one paragraph must be caught");

        let mut guard = LoopGuard::default();
        let mut state = 7_u64;
        for _ in 0..600 {
            let mut text = String::new();
            for _ in 0..8 {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                text.push_str(&format!("word{} ", state >> 40));
            }
            assert!(
                !guard.feed(&chat_line("content", &text)),
                "varied text is not a loop"
            );
        }

        let mut guard = LoopGuard::default();
        for _ in 0..2000 {
            let call = json!({"choices": [{"delta": {"tool_calls": [{"function": {"arguments": "0,0,0,0,"}}]}}]});
            assert!(
                !guard.feed(format!("data: {call}\n\n").as_bytes()),
                "tool arguments are not watched"
            );
        }
    }

    #[test]
    fn only_a_reasoning_only_generation_cut_at_the_limit_counts_as_an_overrun() {
        let finish = |reason: &str| {
            format!(
                "data: {}\n\n",
                json!({"choices": [{"index": 0, "delta": {}, "finish_reason": reason}]})
            )
            .into_bytes()
        };
        let mut guard = LoopGuard::default();
        guard.feed(&chat_line("reasoning_content", "hmm, let me think. "));
        guard.feed(&chat_line("content", "\n\n"));
        assert!(!guard.overran(), "no finish yet");
        guard.feed(&finish("length"));
        assert!(guard.overran());

        let mut guard = LoopGuard::default();
        guard.feed(&chat_line("reasoning_content", "hmm"));
        guard.feed(&finish("stop"));
        assert!(!guard.overran(), "a normal stop is not an overrun");

        let mut guard = LoopGuard::default();
        guard.feed(&chat_line("content", "Here is the file."));
        guard.feed(&finish("length"));
        assert!(!guard.overran(), "an answer was started");

        let mut guard = LoopGuard::default();
        let call =
            json!({"choices": [{"delta": {"tool_calls": [{"function": {"arguments": "{"}}]}}]});
        guard.feed(format!("data: {call}\n\n").as_bytes());
        guard.feed(&finish("length"));
        assert!(!guard.overran(), "a tool call was started");

        let mut guard = LoopGuard::default();
        guard.feed(
            b"data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hm\"}}\n\n",
        );
        guard.feed(
            b"data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"}}\n\n",
        );
        assert!(guard.overran(), "messages protocol");

        let mut guard = LoopGuard::default();
        guard.feed(b"data: {\"type\":\"response.reasoning_text.delta\",\"delta\":\"hm\"}\n\n");
        guard.feed(b"data: {\"type\":\"response.incomplete\"}\n\n");
        assert!(guard.overran(), "responses protocol");
    }

    #[test]
    fn the_ceiling_binds_only_when_the_client_asked_for_no_less() {
        assert!(cap_binds(&json!({"max_tokens": 100}), 100));
        assert!(!cap_binds(&json!({"max_tokens": 77}), 100));
        assert!(!cap_binds(
            &json!({"max_tokens": 100, "max_completion_tokens": 7}),
            100
        ));
        assert!(
            !cap_binds(&json!({"messages": []}), 100),
            "no limit applied"
        );
        assert!(!cap_binds(&json!({"max_tokens": 100}), 0), "ceiling off");
    }

    #[test]
    fn every_request_gets_an_output_ceiling() {
        let mut open = json!({"messages": []});
        cap_output(&mut open, "max_tokens", 100);
        assert_eq!(open["max_tokens"], 100);
        let mut high = json!({"max_tokens": 5000, "max_completion_tokens": 7});
        cap_output(&mut high, "max_tokens", 100);
        assert_eq!(high["max_tokens"], 100);
        assert_eq!(high["max_completion_tokens"], 7);
        let mut responses = json!({"input": []});
        cap_output(&mut responses, "max_output_tokens", 100);
        assert_eq!(responses["max_output_tokens"], 100);
        let mut off = json!({});
        cap_output(&mut off, "max_tokens", 0);
        assert!(off.get("max_tokens").is_none());
    }

    #[test]
    fn late_system_messages_become_user_messages() {
        let mut chat = json!({"messages": [
            {"role": "system", "content": "a"},
            {"role": "user", "content": "b"},
            {"role": "system", "content": "reminder"},
            {"role": "developer", "content": "c"}
        ]});
        demote_late_system(&mut chat);
        let roles: Vec<&str> = chat["messages"]
            .as_array()
            .expect("list")
            .iter()
            .map(|m| m["role"].as_str().expect("role"))
            .collect();
        assert_eq!(roles, ["system", "user", "user", "user"]);

        let mut responses = json!({"instructions": "lead", "input": [
            {"type": "message", "role": "developer", "content": "x"},
            {"type": "function_call_output", "call_id": "1", "output": "ok"}
        ]});
        demote_late_system(&mut responses);
        assert_eq!(responses["input"][0]["role"], "user");

        let mut plain =
            json!({"input": [{"type": "message", "role": "developer", "content": "x"}]});
        demote_late_system(&mut plain);
        assert_eq!(plain["input"][0]["role"], "developer");
    }
}
