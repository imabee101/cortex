use std::collections::HashMap;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use axum::body::Bytes;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header::RETRY_AFTER;
use axum::response::IntoResponse;
use axum::response::Response;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::AppState;
use crate::inference;

const DEDUPE_MS: u64 = 30_000;

#[derive(Clone)]
struct Hit {
    title: String,
    url: String,
    description: String,
}

pub struct SearchState {
    pub breaker: crate::limits::Breaker,
    dedupe: std::sync::Mutex<HashMap<String, (u64, Vec<Hit>)>>,
}

impl Default for SearchState {
    fn default() -> Self {
        Self {
            breaker: crate::limits::Breaker::new(5, 30_000),
            dedupe: std::sync::Mutex::new(HashMap::new()),
        }
    }
}

pub enum Outcome {
    /// Not a search request: forward it as it is.
    Pass,
    /// The request was a web search and this is its finished answer.
    Answer(Response),
    /// Search is not configured: forward this body, which has no search tool.
    Rewritten(Bytes),
}

pub async fn maybe(state: &AppState, user: Uuid, headers: &HeaderMap, body: &Bytes) -> Outcome {
    let Some(mut payload) = inference::decode_body(headers, body.clone())
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    else {
        return Outcome::Pass;
    };
    // The harness replays earlier turns, including the `web_search_call` item this
    // gateway returned; llama-server cannot type it, and its sources already ride on
    // the answer's annotations.
    let replayed = strip_hosted_items(&mut payload);
    if !is_web_search(&payload) {
        return match replayed {
            true => rewritten(&payload),
            false => Outcome::Pass,
        };
    }
    if !configured(state) {
        // The model entry advertises no backend search without a token, but a
        // client with a stale list may still ship the hosted tool: drop it.
        if let Some(tools) = payload
            .get_mut("tools")
            .and_then(|tools| tools.as_array_mut())
        {
            tools.retain(|tool| !is_search_tool(tool));
        }
        return rewritten(&payload);
    }
    Outcome::Answer(run(state, user, payload).await)
}

fn rewritten(payload: &Value) -> Outcome {
    match serde_json::to_vec(payload) {
        Ok(bytes) => Outcome::Rewritten(Bytes::from(bytes)),
        Err(_) => Outcome::Pass,
    }
}

/// Drop hosted `web_search_call` items from a replayed `input`. True when any went.
fn strip_hosted_items(payload: &mut Value) -> bool {
    let Some(items) = payload
        .get_mut("input")
        .and_then(|input| input.as_array_mut())
    else {
        return false;
    };
    let before = items.len();
    items.retain(|item| item.get("type").and_then(|kind| kind.as_str()) != Some("web_search_call"));
    items.len() != before
}

pub fn configured(state: &AppState) -> bool {
    state
        .config
        .brave_token
        .as_deref()
        .is_some_and(|token| !token.is_empty())
}

fn is_search_tool(tool: &Value) -> bool {
    tool.get("type")
        .and_then(|kind| kind.as_str())
        .is_some_and(|kind| kind.contains("web_search"))
}

fn is_web_search(payload: &Value) -> bool {
    payload
        .get("tools")
        .and_then(|tools| tools.as_array())
        .is_some_and(|tools| tools.iter().any(is_search_tool))
}

async fn run(state: &AppState, user: Uuid, mut payload: Value) -> Response {
    if state
        .config
        .brave_token
        .as_deref()
        .is_none_or(|token| token.is_empty())
    {
        return inference_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "search upstream is not configured",
        );
    }
    if let Err(retry) = state.search.breaker.check() {
        return retry_response(StatusCode::SERVICE_UNAVAILABLE, retry as i32);
    }
    if let Err(response) = search_budget(state, user).await {
        return response;
    }
    let stream = payload
        .get("stream")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let question = input_text(&payload);
    let excluded = excluded_domains(&payload);
    let allowed = allowed_domains(&payload);
    rewrite_tools(&mut payload);
    let Some(origin) = state.config.llama_url.clone() else {
        return inference_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "inference upstream is not configured",
        );
    };
    let model = model_of(&payload, &state.config.model_id);
    let mut hits = Vec::new();
    let mut query = question.clone();
    let mut last = Value::Null;
    // Up to three lookups; a model that ends its turn with reasoning only (no answer
    // and no tool call) is sampled again, twice at most, before the fallback text.
    let mut lookups = 0;
    let mut resamples = 0;
    while lookups < 3 {
        payload["stream"] = json!(false);
        last = match post_llama(state, &origin, &payload).await {
            Ok(response) => response,
            Err(response) => return response,
        };
        let calls = web_calls(&last);
        if calls.is_empty() {
            if !has_answer(&last) && resamples < 2 {
                resamples += 1;
                continue;
            }
            return deliver(stream, &model, &query, &hits, last);
        }
        lookups += 1;
        for call in calls {
            if !call.query.is_empty() {
                query = call.query.clone();
            }
            hits = match lookup(state, &query, &allowed, &excluded).await {
                Ok(hits) => hits,
                Err(response) => return response,
            };
            append_output(&mut payload, &call, &hits);
        }
        release_required_tool_choice(&mut payload);
    }
    deliver(stream, &model, &query, &hits, last)
}

/// The turn produced an answer message or a call for the client to run.
fn has_answer(response: &Value) -> bool {
    response
        .get("output")
        .and_then(|value| value.as_array())
        .is_some_and(|items| {
            items.iter().any(|item| {
                matches!(
                    item.get("type").and_then(|kind| kind.as_str()),
                    Some("message" | "function_call")
                )
            })
        })
}

struct Call {
    call_id: String,
    query: String,
    arguments: String,
}

fn web_calls(response: &Value) -> Vec<Call> {
    let Some(output) = response.get("output").and_then(|value| value.as_array()) else {
        return Vec::new();
    };
    let mut calls = Vec::new();
    for item in output {
        if item.get("type").and_then(|value| value.as_str()) != Some("function_call") {
            continue;
        }
        if item.get("name").and_then(|value| value.as_str()) != Some("web_search") {
            continue;
        }
        let call_id = item
            .get("call_id")
            .and_then(|value| value.as_str())
            .unwrap_or("call")
            .to_owned();
        let arguments = match item.get("arguments") {
            Some(Value::String(text)) => text.clone(),
            Some(other) => serde_json::to_string(other).unwrap_or_else(|_| "{}".to_owned()),
            None => "{}".to_owned(),
        };
        let query = argument_query(&Value::String(arguments.clone()));
        calls.push(Call {
            call_id,
            query,
            arguments,
        });
    }
    calls
}

fn argument_query(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        let parsed: Value = serde_json::from_str(text).unwrap_or(Value::Null);
        return parsed
            .get("query")
            .and_then(|item| item.as_str())
            .unwrap_or(text)
            .to_owned();
    }
    value
        .get("query")
        .and_then(|item| item.as_str())
        .unwrap_or("")
        .to_owned()
}

fn append_output(payload: &mut Value, call: &Call, hits: &[Hit]) {
    let output = json!({
        "type": "function_call_output",
        "call_id": call.call_id,
        "output": serde_json::to_string(&hits.iter().map(|hit| json!({
            "title": hit.title,
            "url": hit.url,
            "description": hit.description,
        })).collect::<Vec<_>>()).unwrap_or_else(|_| "[]".to_owned()),
    });
    let call_item = json!({
        "type": "function_call",
        "name": "web_search",
        "call_id": call.call_id,
        "arguments": call.arguments,
    });
    let input = payload
        .as_object_mut()
        .and_then(|object| object.get_mut("input"));
    match input {
        Some(Value::Array(items)) => {
            items.push(call_item);
            items.push(output);
        }
        Some(other) => {
            let previous = input_message(other.take());
            *other = json!([previous, call_item, output]);
        }
        None => {
            if let Some(object) = payload.as_object_mut() {
                object.insert("input".to_owned(), json!([call_item, output]));
            }
        }
    }
}

fn input_message(value: Value) -> Value {
    match value {
        Value::String(text) => json!({"role": "user", "content": text}),
        other => other,
    }
}

fn release_required_tool_choice(payload: &mut Value) {
    if payload.get("tool_choice").and_then(|value| value.as_str()) == Some("required") {
        payload["tool_choice"] = json!("none");
    }
}

async fn post_llama(state: &AppState, origin: &str, payload: &Value) -> Result<Value, Response> {
    if let Err(retry) = state.llama_breaker.check() {
        return Err(retry_response(
            StatusCode::SERVICE_UNAVAILABLE,
            retry as i32,
        ));
    }
    let _permit = inference::acquire_slot(state).await?;
    let url = format!("{}/v1/responses", origin.trim_end_matches('/'));
    let mut payload = payload.clone();
    inference::demote_late_system(&mut payload);
    inference::cap_output(
        &mut payload,
        "max_output_tokens",
        state.config.max_output_tokens,
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
    let upstream = request.send().await.map_err(|err| {
        tracing::warn!(
            upstream = "llama",
            timeout = err.is_timeout(),
            connect = err.is_connect(),
            cause = %crate::upstream_cause(&err),
            "inference upstream request failed"
        );
        state.llama_breaker.failure();
        inference_error(StatusCode::BAD_GATEWAY, "inference upstream is unavailable")
    })?;
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
    if !status.is_success() {
        let bytes = upstream.bytes().await.unwrap_or_default();
        return Err(passthrough(status, retry_after.as_deref(), bytes));
    }
    upstream
        .json()
        .await
        .map_err(|_| inference_error(StatusCode::BAD_GATEWAY, "inference upstream is unavailable"))
}

async fn lookup(
    state: &AppState,
    query: &str,
    allowed: &[String],
    excluded: &[String],
) -> Result<Vec<Hit>, Response> {
    if query.is_empty() {
        return Err(inference_error(
            StatusCode::BAD_REQUEST,
            "search query is empty",
        ));
    }
    let now = unix_ms();
    if let Ok(mut guard) = state.search.dedupe.lock() {
        if let Some((seen, hits)) = guard.get(query)
            && now.saturating_sub(*seen) < DEDUPE_MS
        {
            return Ok(hits.clone());
        }
        guard.retain(|_, (seen, _)| now.saturating_sub(*seen) < DEDUPE_MS);
    }
    if let Ok(Some(wait)) = state.db.bucket_wait(BRAVE_BUCKET).await {
        return Err(retry_response(StatusCode::TOO_MANY_REQUESTS, wait));
    }
    let token = state.config.brave_token.clone().unwrap_or_default();
    let mut url = reqwest::Url::parse(&format!(
        "{}/web/search",
        state.config.brave_base.trim_end_matches('/')
    ))
    .map_err(|_| {
        inference_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "search upstream is not configured",
        )
    })?;
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("count", "5");
    let response = state
        .http
        .get(url)
        .header("X-Subscription-Token", token)
        .send()
        .await
        .map_err(|err| {
            tracing::warn!(
                upstream = "brave",
                timeout = err.is_timeout(),
                connect = err.is_connect(),
                cause = %crate::upstream_cause(&err),
                "search upstream request failed"
            );
            note_failure(state);
            inference_error(StatusCode::BAD_GATEWAY, "search upstream is unavailable")
        })?;
    let status = response.status();
    record_brave_limits(state, response.headers()).await;
    if !status.is_success() {
        tracing::warn!(
            upstream = "brave",
            status = status.as_u16(),
            "search upstream returned an error status"
        );
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        note_failure(state);
        let retry = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok())
            .unwrap_or(1);
        return Err(retry_response(
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            retry,
        ));
    }
    if !status.is_success() {
        note_failure(state);
        return Err(inference_error(
            StatusCode::BAD_GATEWAY,
            "search upstream is unavailable",
        ));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|_| inference_error(StatusCode::BAD_GATEWAY, "search upstream is unavailable"))?;
    state.search.breaker.success();
    let mut hits = Vec::new();
    if let Some(results) = body
        .pointer("/web/results")
        .and_then(|value| value.as_array())
    {
        for result in results {
            let url = result
                .get("url")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_owned();
            if url.is_empty() || !host_allowed(&url, allowed, excluded) {
                continue;
            }
            hits.push(Hit {
                title: result
                    .get("title")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                    .to_owned(),
                url,
                description: result
                    .get("description")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                    .to_owned(),
            });
        }
    }
    if let Ok(mut guard) = state.search.dedupe.lock() {
        guard.insert(query.to_owned(), (now, hits.clone()));
    }
    Ok(hits)
}

fn note_failure(state: &AppState) {
    state.search.breaker.failure();
}

const BRAVE_BUCKET: &str = "brave";

/// Per-user minute rate and daily quota for search, plus the shared minute
/// bucket that guards the one Brave token.
async fn search_budget(state: &AppState, user: Uuid) -> Result<(), Response> {
    let failed = || inference_error(StatusCode::INTERNAL_SERVER_ERROR, "search quota failed");
    let personal = state
        .db
        .hit_rate(&format!("brave:{user}"), 20, 60)
        .await
        .map_err(|_| failed())?;
    if !personal.allowed {
        return Err(retry_response(
            StatusCode::TOO_MANY_REQUESTS,
            personal.retry_after.max(1),
        ));
    }
    let daily = state
        .db
        .usage_hit(user, "search", state.config.daily_search_quota)
        .await
        .map_err(|_| failed())?;
    if !daily.allowed {
        return Err(retry_response(
            StatusCode::TOO_MANY_REQUESTS,
            daily.retry_after,
        ));
    }
    Ok(())
}

/// Brave reports `X-RateLimit-*` as comma-separated values, one per window (per-second
/// first, then the longer plan window). A window whose limit is 0 is not enforced on
/// this plan, so only windows with a limit count; the smallest remaining count among
/// them, with that window's reset, goes in the shared bucket.
async fn record_brave_limits(state: &AppState, headers: &reqwest::header::HeaderMap) {
    let numbers = |name: &str| -> Vec<i64> {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                value
                    .split(',')
                    .filter_map(|part| part.trim().parse().ok())
                    .collect()
            })
            .unwrap_or_default()
    };
    let Some((left, reset_secs)) = tightest_window(
        &numbers("x-ratelimit-limit"),
        &numbers("x-ratelimit-remaining"),
        &numbers("x-ratelimit-reset"),
    ) else {
        return;
    };
    let _ = state
        .db
        .bucket_set(
            BRAVE_BUCKET,
            left.clamp(0, i32::MAX as i64) as i32,
            reset_secs as i32,
        )
        .await;
}

/// `(remaining, reset seconds)` of the enforced window with the least left.
fn tightest_window(limit: &[i64], remaining: &[i64], reset: &[i64]) -> Option<(i64, i64)> {
    remaining
        .iter()
        .copied()
        .enumerate()
        .filter(|(index, _)| limit.get(*index).is_none_or(|limit| *limit > 0))
        .min_by_key(|(_, value)| *value)
        .map(|(index, left)| {
            let reset_secs = reset.get(index).copied().unwrap_or(1);
            (left, reset_secs.clamp(1, 86_400 * 31))
        })
}

fn host_allowed(url: &str, allowed: &[String], excluded: &[String]) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    let host = parsed.host_str().unwrap_or("");
    if excluded.iter().any(|domain| host_matches(host, domain)) {
        return false;
    }
    allowed.is_empty() || allowed.iter().any(|domain| host_matches(host, domain))
}

fn host_matches(host: &str, domain: &str) -> bool {
    let domain = domain.trim_start_matches('.');
    host == domain || host.ends_with(&format!(".{domain}"))
}

fn rewrite_tools(payload: &mut Value) {
    let Some(tools) = payload
        .get_mut("tools")
        .and_then(|tools| tools.as_array_mut())
    else {
        return;
    };
    if tools.is_empty() {
        return;
    }
    let mut only_search = true;
    for tool in tools.iter_mut() {
        let kind = tool
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        if kind.contains("web_search") {
            *tool = json!({
                "type": "function",
                "name": "web_search",
                "parameters": {
                    "type": "object",
                    "properties": { "query": { "type": "string" } },
                    "required": ["query"]
                }
            });
        } else {
            only_search = false;
        }
    }
    if only_search {
        payload["tool_choice"] = json!("required");
    }
}

fn input_text(payload: &Value) -> String {
    match payload.get("input") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .rev()
            .find_map(|item| {
                item.get("content")
                    .and_then(|content| content.as_str())
                    .or_else(|| item.as_str())
                    .map(str::to_owned)
            })
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn domain_list(payload: &Value, key: &str) -> Vec<String> {
    payload
        .get("tools")
        .and_then(|tools| tools.as_array())
        .and_then(|tools| tools.first())
        .and_then(|tool| tool.pointer(&format!("/filters/{key}")))
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn excluded_domains(payload: &Value) -> Vec<String> {
    domain_list(payload, "excluded_domains")
}

fn allowed_domains(payload: &Value) -> Vec<String> {
    domain_list(payload, "allowed_domains")
}

fn deliver(stream: bool, model: &str, query: &str, hits: &[Hit], response: Value) -> Response {
    let body = prepare_response(model, query, hits, response);
    if stream {
        let bytes = render_sse(&body);
        return axum::response::Response::builder()
            .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
            .header(axum::http::header::CACHE_CONTROL, "no-cache")
            .body(axum::body::Body::from(bytes))
            .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response());
    }
    axum::Json(body).into_response()
}

fn prepare_response(model: &str, query: &str, hits: &[Hit], mut response: Value) -> Value {
    if !response.is_object() {
        response = json!({});
    }
    let object = response.as_object_mut().expect("response object");
    object
        .entry("id")
        .or_insert_with(|| json!(format!("resp_{}", Uuid::new_v4())));
    object.insert("object".to_owned(), json!("response"));
    object
        .entry("created_at")
        .or_insert(json!(unix_ms() / 1000));
    object.insert("status".to_owned(), json!("completed"));
    object.entry("model").or_insert(json!(model));
    let mut output = match object.remove("output") {
        Some(Value::Array(items)) => items,
        _ => Vec::new(),
    };
    output.retain(|item| {
        !(item.get("type").and_then(|value| value.as_str()) == Some("function_call")
            && item.get("name").and_then(|value| value.as_str()) == Some("web_search"))
    });
    for item in &mut output {
        normalize_item(item);
    }
    let has_message = output
        .iter()
        .any(|item| item.get("type").and_then(|value| value.as_str()) == Some("message"));
    let has_client_call = output
        .iter()
        .any(|item| item.get("type").and_then(|value| value.as_str()) == Some("function_call"));
    if !has_message && !has_client_call {
        // A turn that ended inside its reasoning still holds the model's conclusion;
        // only without one does the answer fall back to the results themselves.
        let text = reasoning_conclusion(&output).unwrap_or_else(|| fallback_text(hits, query));
        output.push(message_item(&text, hits));
    } else if !hits.is_empty() {
        add_citations(&mut output, hits);
    }
    if !hits.is_empty() {
        output.insert(0, web_search_item(query, hits));
    }
    object.insert("output".to_owned(), Value::Array(output));
    ensure_usage(object);
    response
}

fn normalize_item(item: &mut Value) {
    let Some(map) = item.as_object_mut() else {
        return;
    };
    let kind = map
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_owned();
    match kind.as_str() {
        "function_call" => {
            let arguments = map.get("arguments").cloned().unwrap_or(json!(""));
            if !arguments.is_string() {
                map.insert(
                    "arguments".to_owned(),
                    Value::String(
                        serde_json::to_string(&arguments).unwrap_or_else(|_| "{}".to_owned()),
                    ),
                );
            }
            if !map.contains_key("call_id") {
                let id = map
                    .get("id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("call")
                    .to_owned();
                map.insert("call_id".to_owned(), json!(id));
            }
            map.entry("name").or_insert(json!("tool"));
        }
        "message" => {
            map.entry("id").or_insert(json!("msg_1"));
            map.insert("role".to_owned(), json!("assistant"));
            map.entry("status").or_insert(json!("completed"));
            if let Some(content) = map
                .get_mut("content")
                .and_then(|value| value.as_array_mut())
            {
                for part in content {
                    if let Some(part) = part.as_object_mut()
                        && part.get("type").and_then(|value| value.as_str()) == Some("output_text")
                    {
                        part.entry("annotations").or_insert(json!([]));
                    }
                }
            }
        }
        "reasoning" => {
            map.entry("id").or_insert(json!("rs_1"));
            map.entry("summary").or_insert(json!([]));
        }
        _ => {}
    }
}

fn add_citations(output: &mut [Value], hits: &[Hit]) {
    let annotations = citations(hits, 1);
    for item in output {
        if item.get("type").and_then(|value| value.as_str()) != Some("message") {
            continue;
        }
        let Some(content) = item
            .get_mut("content")
            .and_then(|value| value.as_array_mut())
        else {
            continue;
        };
        for part in content {
            if part.get("type").and_then(|value| value.as_str()) == Some("output_text") {
                part.as_object_mut()
                    .map(|part| part.insert("annotations".to_owned(), json!(annotations.clone())));
            }
        }
    }
}

fn citations(hits: &[Hit], end: usize) -> Vec<Value> {
    hits.iter()
        .map(|hit| {
            json!({
                "type": "url_citation",
                "url": hit.url,
                "title": hit.title,
                "start_index": 0,
                "end_index": end,
            })
        })
        .collect()
}

fn message_item(text: &str, hits: &[Hit]) -> Value {
    let end = if text.is_empty() {
        0
    } else {
        1.min(text.len())
    };
    json!({
        "type": "message",
        "id": "msg_1",
        "status": "completed",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": text,
            "annotations": citations(hits, end),
        }]
    })
}

fn web_search_item(query: &str, hits: &[Hit]) -> Value {
    json!({
        "type": "web_search_call",
        "id": format!("ws_{}", Uuid::new_v4()),
        "status": "completed",
        "action": {
            "type": "search",
            "query": query,
            "sources": hits.iter().map(|hit| json!({"type": "url", "url": hit.url})).collect::<Vec<_>>()
        }
    })
}

fn ensure_usage(object: &mut serde_json::Map<String, Value>) {
    let usage = object.entry("usage").or_insert_with(|| json!({}));
    let Some(usage) = usage.as_object_mut() else {
        return;
    };
    usage.entry("input_tokens").or_insert(json!(0));
    usage.entry("output_tokens").or_insert(json!(0));
    usage.entry("total_tokens").or_insert(json!(0));
    usage
        .entry("input_tokens_details")
        .or_insert(json!({"cached_tokens": 0}));
    usage
        .entry("output_tokens_details")
        .or_insert(json!({"reasoning_tokens": 0}));
}

fn render_sse(response: &Value) -> String {
    let mut events = Vec::new();
    let mut seq = 0u64;
    let output = response
        .get("output")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    for (index, item) in output.iter().enumerate() {
        let index = index as u64;
        let kind = item
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let id = item
            .get("id")
            .and_then(|value| value.as_str())
            .unwrap_or("item")
            .to_owned();
        push_event(
            &mut events,
            &mut seq,
            "response.output_item.added",
            json!({
                "output_index": index,
                "item": item,
            }),
        );
        match kind {
            "message" => emit_message(&mut events, &mut seq, index, &id, item),
            "function_call" => emit_function(&mut events, &mut seq, index, &id, item),
            "reasoning" => emit_reasoning(&mut events, &mut seq, index, &id, item),
            "web_search_call" => {
                for name in [
                    "response.web_search_call.in_progress",
                    "response.web_search_call.searching",
                    "response.web_search_call.completed",
                ] {
                    push_event(
                        &mut events,
                        &mut seq,
                        name,
                        json!({
                            "output_index": index,
                            "item_id": id,
                        }),
                    );
                }
            }
            _ => {}
        }
        push_event(
            &mut events,
            &mut seq,
            "response.output_item.done",
            json!({
                "output_index": index,
                "item": item,
            }),
        );
    }
    push_event(
        &mut events,
        &mut seq,
        "response.completed",
        json!({ "response": response }),
    );
    let mut body = String::new();
    for (name, data) in events {
        body.push_str("event: ");
        body.push_str(&name);
        body.push('\n');
        body.push_str("data: ");
        body.push_str(&data.to_string());
        body.push_str("\n\n");
    }
    body
}

fn emit_message(
    events: &mut Vec<(String, Value)>,
    seq: &mut u64,
    index: u64,
    id: &str,
    item: &Value,
) {
    let Some(content) = item.get("content").and_then(|value| value.as_array()) else {
        return;
    };
    for (content_index, part) in content.iter().enumerate() {
        if part.get("type").and_then(|value| value.as_str()) != Some("output_text") {
            continue;
        }
        let text = part
            .get("text")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        push_event(
            events,
            seq,
            "response.content_part.added",
            json!({
                "item_id": id,
                "output_index": index,
                "content_index": content_index,
                "part": part,
            }),
        );
        push_event(
            events,
            seq,
            "response.output_text.delta",
            json!({
                "item_id": id,
                "output_index": index,
                "content_index": content_index,
                "delta": text,
            }),
        );
        push_event(
            events,
            seq,
            "response.output_text.done",
            json!({
                "item_id": id,
                "output_index": index,
                "content_index": content_index,
                "text": text,
            }),
        );
    }
}

fn emit_function(
    events: &mut Vec<(String, Value)>,
    seq: &mut u64,
    index: u64,
    id: &str,
    item: &Value,
) {
    let arguments = item
        .get("arguments")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    push_event(
        events,
        seq,
        "response.function_call_arguments.delta",
        json!({
            "item_id": id,
            "output_index": index,
            "delta": arguments,
        }),
    );
    push_event(
        events,
        seq,
        "response.function_call_arguments.done",
        json!({
            "item_id": id,
            "output_index": index,
            "arguments": arguments,
            "name": item.get("name").and_then(|value| value.as_str()).unwrap_or(""),
        }),
    );
}

fn emit_reasoning(
    events: &mut Vec<(String, Value)>,
    seq: &mut u64,
    index: u64,
    id: &str,
    item: &Value,
) {
    let text = item
        .get("content")
        .and_then(|value| value.as_array())
        .and_then(|content| content.first())
        .and_then(|part| part.get("text"))
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if text.is_empty() {
        return;
    }
    push_event(
        events,
        seq,
        "response.reasoning_text.delta",
        json!({
            "item_id": id,
            "output_index": index,
            "content_index": 0,
            "delta": text,
        }),
    );
    push_event(
        events,
        seq,
        "response.reasoning_text.done",
        json!({
            "item_id": id,
            "output_index": index,
            "content_index": 0,
            "text": text,
        }),
    );
}

fn push_event(events: &mut Vec<(String, Value)>, seq: &mut u64, name: &str, mut data: Value) {
    *seq += 1;
    if let Some(object) = data.as_object_mut() {
        object.insert("type".to_owned(), json!(name));
        object.insert("sequence_number".to_owned(), json!(*seq));
    }
    events.push((name.to_owned(), data));
}

/// The closing statement of the last reasoning item: the text after its last
/// `Answer:` marker, else its last paragraph.
fn reasoning_conclusion(output: &[Value]) -> Option<String> {
    let item = output
        .iter()
        .rev()
        .find(|item| item.get("type").and_then(|kind| kind.as_str()) == Some("reasoning"))?;
    let text = item
        .get("content")
        .and_then(|content| content.as_array())?
        .iter()
        .filter_map(|part| part.get("text").and_then(|text| text.as_str()))
        .collect::<Vec<_>>()
        .join("\n\n");
    let conclusion = match text.rfind("Answer:") {
        Some(at) => &text[at + "Answer:".len()..],
        None => text.rsplit("\n\n").find(|part| !part.trim().is_empty())?,
    };
    let conclusion = conclusion.trim();
    (!conclusion.is_empty() && conclusion.len() <= 2000).then(|| conclusion.to_owned())
}

fn fallback_text(hits: &[Hit], question: &str) -> String {
    if hits.is_empty() {
        return format!("No results for {question}");
    }
    hits.iter()
        .map(|hit| format!("{} {}", hit.title, hit.description))
        .collect::<Vec<_>>()
        .join(" ")
}

fn model_of(payload: &Value, fallback: &str) -> String {
    payload
        .get("model")
        .and_then(|value| value.as_str())
        .unwrap_or(fallback)
        .to_owned()
}

fn inference_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        axum::Json(json!({"error": {"message": message, "type": "server_error"}})),
    )
        .into_response()
}

fn retry_response(status: StatusCode, retry_after: i32) -> Response {
    (
        status,
        [(RETRY_AFTER, retry_after.to_string())],
        axum::Json(
            json!({"error": {"message": "search upstream is unavailable", "type": "server_error"}}),
        ),
    )
        .into_response()
}

fn passthrough(status: reqwest::StatusCode, retry_after: Option<&str>, bytes: Bytes) -> Response {
    let mut response = axum::response::Response::builder()
        .status(status.as_u16())
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response());
    if let Some(retry_after) = retry_after
        && let Ok(value) = axum::http::HeaderValue::from_str(retry_after)
    {
        response.headers_mut().insert(RETRY_AFTER, value);
    }
    response
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamed_client_tool_call_ends_with_response_completed() {
        let response = json!({
            "output": [{
                "type": "function_call",
                "name": "write",
                "call_id": "call_w",
                "arguments": {"path": "hello.py", "content": "print(\"hi\")"}
            }]
        });
        let body = prepare_response("Qwen3.5-9B", "", &[], response);
        let sse = render_sse(&body);
        assert!(sse.contains("response.completed"), "{sse}");
        assert!(sse.contains("\"sequence_number\":"), "{sse}");
        assert!(sse.contains("hello.py"), "{sse}");
        assert!(sse.contains("call_w"), "{sse}");
        let arguments = body["output"][0]["arguments"]
            .as_str()
            .expect("arguments string");
        assert!(arguments.contains("hello.py"), "{arguments}");
    }

    #[test]
    fn an_unenforced_window_does_not_empty_the_bucket() {
        // Observed from the live API: "50, 0" limits, "49, 0" remaining, "1, 2083912" reset.
        assert_eq!(
            tightest_window(&[50, 0], &[49, 0], &[1, 2_083_912]),
            Some((49, 1))
        );
        assert_eq!(
            tightest_window(&[50, 1000], &[49, 3], &[1, 90]),
            Some((3, 90))
        );
        assert_eq!(tightest_window(&[], &[7], &[]), Some((7, 1)));
        assert_eq!(tightest_window(&[0], &[0], &[5]), None);
    }

    #[test]
    fn a_reasoning_only_turn_answers_with_its_conclusion() {
        let marked = json!([{"type": "reasoning", "content": [
            {"type": "reasoning_text", "text": "Notes.\n\nAnswer: Version 1.99.0 (https://releases.rs/)."}]}]);
        assert_eq!(
            reasoning_conclusion(marked.as_array().expect("array")).as_deref(),
            Some("Version 1.99.0 (https://releases.rs/).")
        );
        let plain = json!([{"type": "reasoning", "content": [
            {"type": "reasoning_text", "text": "Thinking.\n\nIt is 1.99.0."}]}]);
        assert_eq!(
            reasoning_conclusion(plain.as_array().expect("array")).as_deref(),
            Some("It is 1.99.0.")
        );
        assert_eq!(reasoning_conclusion(&[json!({"type": "message"})]), None);
    }

    #[test]
    fn a_reasoning_only_turn_is_not_an_answer() {
        assert!(!has_answer(&json!({"output": [{"type": "reasoning"}]})));
        assert!(has_answer(
            &json!({"output": [{"type": "reasoning"}, {"type": "message"}]})
        ));
        assert!(has_answer(&json!({"output": [{"type": "function_call"}]})));
        assert!(!has_answer(&json!({})));
    }

    #[test]
    fn replayed_web_search_calls_are_dropped_from_input() {
        let mut payload = json!({
            "input": [
                {"role": "user", "content": "q"},
                {"type": "web_search_call", "id": "ws_1", "status": "completed"},
                {"type": "message", "role": "assistant", "content": []}
            ]
        });
        assert!(strip_hosted_items(&mut payload));
        let kinds: Vec<_> = payload["input"]
            .as_array()
            .expect("input")
            .iter()
            .map(|item| item.get("type").and_then(|kind| kind.as_str()))
            .collect();
        assert_eq!(kinds, vec![None, Some("message")]);
        assert!(!strip_hosted_items(&mut payload));
        assert!(!strip_hosted_items(&mut json!({"input": "text"})));
    }

    #[test]
    fn string_input_follow_up_is_typed() {
        let mut payload = json!({
            "input": "official website",
            "tools": [{"type": "web_search"}]
        });
        rewrite_tools(&mut payload);
        assert_eq!(payload["tool_choice"], "required");
        assert_eq!(payload["tools"][0]["type"], "function");
        let call = Call {
            call_id: "call_1".to_owned(),
            query: "official website".to_owned(),
            arguments: "{\"query\":\"official website\"}".to_owned(),
        };
        let hits = vec![Hit {
            title: "Rust".to_owned(),
            url: "https://www.rust-lang.org/".to_owned(),
            description: "site".to_owned(),
        }];
        append_output(&mut payload, &call, &hits);
        release_required_tool_choice(&mut payload);
        let input = payload["input"].as_array().expect("input array");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"], "official website");
        assert_eq!(input[1]["type"], "function_call");
        assert!(input[1]["arguments"].is_string());
        assert_eq!(input[2]["type"], "function_call_output");
        assert!(input[2]["output"].is_string());
        assert_eq!(payload["tool_choice"], "none");
    }

    #[test]
    fn search_answer_keeps_the_citation_and_drops_the_server_call() {
        let hits = vec![Hit {
            title: "Rust".to_owned(),
            url: "https://www.rust-lang.org/".to_owned(),
            description: "A language".to_owned(),
        }];
        let response = json!({
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Ownership is a Rust rule."}]
            }]
        });
        let body = prepare_response("Qwen3.5-9B", "rust ownership", &hits, response);
        let rendered = body.to_string();
        assert!(
            rendered.contains("https://www.rust-lang.org/"),
            "{rendered}"
        );
        assert!(rendered.contains("Ownership is a Rust rule."), "{rendered}");
        assert_eq!(body["output"][0]["type"], "web_search_call");
        assert!(body["usage"]["output_tokens_details"]["reasoning_tokens"].is_number());
    }
}
