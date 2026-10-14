// Test clients talk to loopback mocks only.
#![allow(clippy::disallowed_methods)]
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::Request;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::routing::post;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cortex_api::jwt::b64url;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const DB: &str = "postgres://127.0.0.1/cortex_api_infer";

#[tokio::test]
async fn inference_normalizes_llama_shapes() {
    let llama = mock_llama().await;
    let probe = reqwest::Client::new();
    for _ in 0..50 {
        if probe
            .get(format!("{llama}/props"))
            .send()
            .await
            .ok()
            .is_some_and(|r| r.status().is_success())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    truncate();
    let mut config = cortex_api::Config::new("127.0.0.1:0".parse().expect("addr"), DB.to_owned());
    config.llama_url = Some(llama);
    config.reasoning_budgets = Some(
        cortex_api::ReasoningBudgets::parse("low=2048,medium=8192", "medium").expect("budgets"),
    );
    let running = cortex_api::serve(config).await.expect("serve");
    let base = format!("http://{}", running.addr);
    let http = reqwest::Client::builder()
        .cookie_store(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .expect("client");

    let (verifier, challenge) = pkce();
    let redirect = "http://127.0.0.1:1/callback";
    let authorize = format!(
        "{base}/authorize?response_type=code&client_id={client}&redirect_uri={redirect}&scope=openid&code_challenge={challenge}&code_challenge_method=S256&state=inf&nonce=inf-nonce",
        client = cortex_api::CLIENT_ID,
        redirect = urlencoding(redirect),
        challenge = urlencoding(&challenge),
    );
    let next = authorize.trim_start_matches(&base);
    http.get(&authorize).send().await.expect("authorize");
    let created = http
        .post(format!("{base}/register"))
        .form(&[
            ("email", "inf@example.com"),
            ("password", "correct-horse"),
            ("first_name", "Inf"),
            ("last_name", "Test"),
            ("next", next),
        ])
        .send()
        .await
        .expect("register");
    assert_eq!(created.status(), 303);
    let allow = http
        .post(format!("{base}/consent"))
        .form(&[("state", "inf"), ("decision", "allow")])
        .send()
        .await
        .expect("allow");
    assert_eq!(allow.status(), 303);
    let loc = allow
        .headers()
        .get(reqwest::header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap();
    let code = query_param(loc, "code").expect("code");
    let tokens: Value = http
        .post(format!("{base}/oauth2/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect),
            ("client_id", cortex_api::CLIENT_ID),
            ("code_verifier", verifier.as_str()),
        ])
        .send()
        .await
        .expect("token")
        .json()
        .await
        .expect("token json");
    let access = tokens["access_token"].as_str().expect("access");

    let models: Value = http
        .get(format!("{base}/v1/models"))
        .bearer_auth(access)
        .send()
        .await
        .expect("models")
        .json()
        .await
        .expect("models json");
    assert_eq!(models["data"][0]["context_window"], 4096);
    // The harness's effort menu, with the level it selects by default.
    assert_eq!(models["data"][0]["supports_reasoning_effort"], true);
    assert_eq!(
        models["data"][0]["reasoning_efforts"],
        json!([
            {"value": "none", "default": false},
            {"value": "minimal", "default": false},
            {"value": "low", "default": false},
            {"value": "medium", "default": true},
            {"value": "high", "default": false},
            {"value": "xhigh", "default": false},
            {"value": "max", "default": false}
        ])
    );

    let chat = http
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(access)
        .json(&json!({"model": "Qwen3.5-9B", "stream": true, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .expect("chat");
    assert_eq!(chat.status(), 200);
    assert_eq!(chat.headers()["x-cortex-context-window"], "4096");
    let chat_body = chat.text().await.expect("chat text");
    assert!(
        chat_body.contains("\\\"cmd\\\":\\\"ls\\\"")
            || chat_body.contains("\"arguments\":\"{\\\"cmd\\\":\\\"ls\\\"}\"")
    );
    assert!(chat_body.contains("[DONE]"));

    // An overflow reaches the client as llama-server sent it, reporting half the window so that the client's
    // own count, which runs below llama's, is over it and the client compacts and resubmits.
    let overflow = http
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(access)
        .json(&json!({"model": "overflow", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .expect("overflow");
    assert_eq!(overflow.status(), 400);
    assert_eq!(overflow.headers()["x-cortex-context-window"], "2048");
    let overflow: Value = overflow.json().await.expect("overflow json");
    assert_eq!(overflow["error"]["type"], "exceed_context_size_error");

    // No output limit is added: a reply ends when the model stops or the context is full, which is
    // what the harness assumes of a length finish. A limit the client sets itself is passed on.
    assert_eq!(LAST_MAX_TOKENS.load(std::sync::atomic::Ordering::SeqCst), 0);
    http.post(format!("{base}/v1/chat/completions"))
        .bearer_auth(access)
        .json(&json!({"model": "m", "max_tokens": 77, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .expect("chat low")
        .bytes()
        .await
        .expect("chat low body");
    assert_eq!(
        LAST_MAX_TOKENS.load(std::sync::atomic::Ordering::SeqCst),
        77
    );

    // llama-server receives the thinking budget of the level the harness sent, and the default
    // level's budget when the request names no level.
    for (body, budget) in [
        (
            json!({"model": "m", "reasoning_effort": "low", "messages": [{"role": "user", "content": "hi"}]}),
            2048,
        ),
        (
            json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}),
            8192,
        ),
    ] {
        http.post(format!("{base}/v1/chat/completions"))
            .bearer_auth(access)
            .json(&body)
            .send()
            .await
            .expect("chat budget")
            .bytes()
            .await
            .expect("chat budget body");
        assert_eq!(
            LAST_REASONING_BUDGET.load(std::sync::atomic::Ordering::SeqCst),
            budget
        );
    }

    // A generation that only repeats itself is cut, the slot comes back, and the upstream is dropped.
    let looping = http
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(access)
        .json(&json!({"model": "loop", "stream": true, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .expect("loop");
    assert_eq!(looping.status(), 200);
    assert!(
        looping.bytes().await.is_err(),
        "the stream must end in an error the client retries"
    );
    for _ in 0..100 {
        if LOOP_DROPPED.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        LOOP_DROPPED.load(std::sync::atomic::Ordering::SeqCst),
        "upstream dropped"
    );
    let metrics = http
        .get(format!("{base}/metrics"))
        .send()
        .await
        .expect("metrics")
        .text()
        .await
        .expect("metrics text");
    assert!(
        metrics.contains("cortex_api_loops_aborted_total 1"),
        "{metrics}"
    );
    assert!(
        metrics.contains("cortex_api_inference_in_flight 0"),
        "{metrics}"
    );

    // A length finish, with or without an answer, passes through untouched.
    for model in ["longanswer", "reasoningonly"] {
        let kept = http
            .post(format!("{base}/v1/chat/completions"))
            .bearer_auth(access)
            .json(&json!({"model": model, "stream": true, "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await
            .expect("kept")
            .text()
            .await
            .expect("a finish at the limit passes through untouched");
        assert!(
            kept.contains("\"finish_reason\":\"length\""),
            "{model}: {kept}"
        );
    }

    // The served model has no vision: images in any shape get the coded rejection the client
    // recovers from (drop the image, resend), and never reach llama.
    let png = "data:image/png;base64,iVBORw0KGgo=";
    for (path, body) in [
        (
            "chat/completions",
            json!({"model": "m", "messages": [{"role": "tool", "tool_call_id": "t", "content": [
                {"type": "text", "text": "file"},
                {"type": "image_url", "image_url": {"url": png}}]}]}),
        ),
        (
            "responses",
            json!({"model": "m", "input": [{"role": "user", "content": [
                {"type": "input_image", "image_url": png}]}]}),
        ),
        (
            "messages",
            json!({"model": "m", "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="}}]}]}),
        ),
    ] {
        let before = UPSTREAM_CALLS.load(std::sync::atomic::Ordering::SeqCst);
        let rejected = http
            .post(format!("{base}/v1/{path}"))
            .bearer_auth(access)
            .json(&body)
            .send()
            .await
            .expect("image request");
        assert_eq!(rejected.status(), 400, "{path}");
        let error: Value = rejected.json().await.expect("error json");
        assert_eq!(error["error"]["code"], "invalid_image", "{path}");
        assert_eq!(
            UPSTREAM_CALLS.load(std::sync::atomic::Ordering::SeqCst),
            before,
            "{path} reached llama"
        );
    }
    // A call forced to one function reaches llama in the string form it honors.
    let tool = |name: &str| json!({"type": "function", "function": {"name": name, "parameters": {"type": "object"}}});
    http.post(format!("{base}/v1/chat/completions"))
        .bearer_auth(access)
        .json(
            &json!({"model": "forced", "messages": [{"role": "user", "content": "hi"}],
            "tools": [tool("session_title"), tool("other")],
            "tool_choice": {"type": "function", "function": {"name": "session_title"}}}),
        )
        .send()
        .await
        .expect("forced")
        .bytes()
        .await
        .expect("forced body");
    let sent = LAST_TOOLING.lock().expect("lock").clone();
    assert_eq!(sent["tool_choice"], "required");
    assert_eq!(sent["tools"], json!([tool("session_title")]));

    // A tool schema that names an image property is not image content.
    let schema = http
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(access)
        .json(
            &json!({"model": "m", "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "function", "function": {"name": "f", "parameters":
                {"type": "object", "properties": {"image": {"type": "image"}}}}}]}),
        )
        .send()
        .await
        .expect("schema");
    assert_eq!(schema.status(), 200);

    // A client that stops sending its body mid-request (the harness cancelling a side call at shutdown) is
    // answered 499, not counted as a request the server refused.
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut socket = tokio::net::TcpStream::connect(running.addr)
            .await
            .expect("connect");
        socket
            .write_all(
                format!(
                    "POST /v1/chat/completions HTTP/1.1\r\nhost: x\r\nauthorization: Bearer {access}\r\ncontent-type: application/json\r\ncontent-length: 1000\r\n\r\n{{\"model\":"
                )
                .as_bytes(),
            )
            .await
            .expect("write");
        socket.shutdown().await.expect("half-close");
        let mut answer = String::new();
        socket.read_to_string(&mut answer).await.expect("read");
        assert!(answer.starts_with("HTTP/1.1 499"), "{answer}");
    }

    let responses = http
        .post(format!("{base}/v1/responses"))
        .bearer_auth(access)
        .json(&json!({"model": "Qwen3.5-9B", "stream": true, "input": "hi"}))
        .send()
        .await
        .expect("responses");
    let responses_body = responses.text().await.expect("responses text");
    assert!(responses_body.contains("\"content_index\":0"));
    assert!(responses_body.contains("think"));

    let limited = http
        .post(format!("{base}/v1/messages"))
        .bearer_auth(access)
        .json(&json!({"model": "limit", "messages": []}))
        .send()
        .await
        .expect("limited");
    assert_eq!(limited.status(), 429);
    assert_eq!(
        limited
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok()),
        Some("3")
    );
    assert!(running.addr.port() > 0);
}

async fn mock_llama() -> String {
    let app = Router::new()
        .route("/props", get(props))
        .route("/v1/chat/completions", post(chat))
        .route("/v1/responses", post(responses))
        .route("/v1/messages", post(messages));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

async fn props() -> impl IntoResponse {
    Json(json!({"default_generation_settings": {"n_ctx": 4096}}))
}

static LAST_TOOLING: std::sync::Mutex<Value> = std::sync::Mutex::new(Value::Null);
static UPSTREAM_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LAST_MAX_TOKENS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LAST_REASONING_BUDGET: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LOOP_DROPPED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

struct DropFlag;
impl Drop for DropFlag {
    fn drop(&mut self) {
        LOOP_DROPPED.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

async fn chat(request: Request) -> axum::response::Response {
    UPSTREAM_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .unwrap_or_default();
    let payload: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    LAST_MAX_TOKENS.store(
        payload["max_tokens"].as_u64().unwrap_or(0),
        std::sync::atomic::Ordering::SeqCst,
    );
    LAST_REASONING_BUDGET.store(
        payload["reasoning_budget_tokens"].as_u64().unwrap_or(0),
        std::sync::atomic::Ordering::SeqCst,
    );
    if payload["model"] == "overflow" {
        // llama-server's own answer to a prompt longer than its context.
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"error": {"code": 400, "message": "request (5000 tokens) exceeds the available context size (4096 tokens), try increasing it", "type": "exceed_context_size_error", "n_prompt_tokens": 5000, "n_ctx": 4096}})),
        )
            .into_response();
    }
    if payload["model"] == "forced" {
        *LAST_TOOLING.lock().expect("lock") =
            json!({"tool_choice": payload["tool_choice"], "tools": payload["tools"]});
    }
    if payload["model"] == "loop" {
        // Never ends on its own: the same sentence for as long as the client listens.
        let endless = futures_util::stream::unfold(DropFlag, |flag| async move {
            tokio::time::sleep(Duration::from_millis(1)).await;
            let line = "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"Let me check the file again to make sure it is right. \"}}]}\n\n";
            Some((Ok::<_, std::io::Error>(axum::body::Bytes::from(line)), flag))
        });
        return (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            axum::body::Body::from_stream(endless),
        )
            .into_response();
    }
    if payload["model"] == "reasoningonly" || payload["model"] == "longanswer" {
        let field = if payload["model"] == "reasoningonly" {
            "reasoning_content"
        } else {
            "content"
        };
        let chunks = vec![
            format!("data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"{field}\":\"thinking it over\"}}}}]}}\n\n"),
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}]}\n\ndata: [DONE]\n\n".to_owned(),
        ];
        let paced = futures_util::stream::unfold(chunks.into_iter(), |mut rest| async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let next = rest.next()?;
            Some((Ok::<_, std::io::Error>(axum::body::Bytes::from(next)), rest))
        });
        return (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            axum::body::Body::from_stream(paced),
        )
            .into_response();
    }
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"function\":{\"name\":\"shell\",\"arguments\":{\"cmd\":\"ls\"}}}]}}]}\n\ndata: [DONE]\n\n",
    )
        .into_response()
}

async fn responses(request: Request) -> impl IntoResponse {
    UPSTREAM_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let _ = axum::body::to_bytes(request.into_body(), 1024 * 1024).await;
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        "data: {\"type\":\"response.reasoning_text.delta\",\"delta\":\"think\",\"item_id\":\"rs\",\"output_index\":0,\"sequence_number\":1}\n\ndata: [DONE]\n\n",
    )
}

async fn messages(request: Request) -> axum::response::Response {
    UPSTREAM_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .unwrap_or_default();
    let payload: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    if payload["model"] == "limit" {
        return (
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            [(axum::http::header::RETRY_AFTER, "3")],
            Json(json!({"error": {"message": "busy", "type": "rate_limit"}})),
        )
            .into_response();
    }
    Json(json!({"content": [{"type": "text", "text": "ok"}]})).into_response()
}

fn pkce() -> (String, String) {
    let mut raw = [0u8; 32];
    rand::Rng::fill(&mut rand::rng(), &mut raw);
    let verifier = URL_SAFE_NO_PAD.encode(raw);
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn urlencoding(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let (_, query) = url.split_once('?')?;
    for pair in query.split('&') {
        let (k, _) = pair.split_once('=')?;
        if k == key {
            return url::form_urlencoded::parse(pair.as_bytes())
                .next()
                .map(|(_, value)| value.into_owned());
        }
    }
    None
}

fn truncate() {
    let _ = std::process::Command::new("psql")
        .args([
            "-h",
            "127.0.0.1",
            "-d",
            "cortex_api_infer",
            "-c",
            "TRUNCATE users, browser_sessions, auth_transactions, refresh_tokens, device_grants, login_attempts, rate_buckets CASCADE",
        ])
        .status();
}
