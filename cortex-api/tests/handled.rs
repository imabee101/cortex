//! One pass through the API: reasoning levels become llama.cpp budgets, and the
//! search response is the body the client already deserializes.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::routing::post;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const DB: &str = "cortex_api_handled";
const BRAVE: &str = "handled-brave-token";

#[tokio::test]
async fn reasoning_budgets_and_search_body_match_the_client() {
    ensure_db();
    let chats = Arc::new(Mutex::new(Vec::new()));
    let llama = mock_llama(chats.clone()).await;
    let brave = mock_brave().await;
    truncate();
    let mut config = cortex_api::Config::new(
        "127.0.0.1:0".parse().expect("addr"),
        format!("postgres://127.0.0.1/{DB}"),
    );
    config.llama_url = Some(llama);
    config.brave_token = Some(BRAVE.to_owned());
    config.brave_base = brave;
    let running = cortex_api::serve(config).await.expect("serve");
    let base = format!("http://{}", running.addr);
    let http = reqwest::Client::builder()
        .cookie_store(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .expect("client");
    let access = sign_in(&http, &base).await;

    let models: Value = http
        .get(format!("{base}/v1/models"))
        .bearer_auth(&access)
        .send()
        .await
        .expect("models")
        .json()
        .await
        .expect("models json");
    assert_eq!(models["data"][0]["supports_reasoning_effort"], true);
    assert_eq!(models["data"][0]["supports_backend_search"], false);
    let levels: Vec<&str> = models["data"][0]["reasoning_efforts"]
        .as_array()
        .expect("menu")
        .iter()
        .filter_map(|item| item["value"].as_str())
        .collect();
    assert!(levels.contains(&"none") && levels.contains(&"low"), "{levels:?}");

    for effort in ["none", "low"] {
        let status = http
            .post(format!("{base}/v1/chat/completions"))
            .bearer_auth(&access)
            .json(&json!({
                "model": "Qwen3.5-9B",
                "stream": false,
                "reasoning_effort": effort,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await
            .expect("chat")
            .status();
        assert_eq!(status, 200, "{effort}");
    }
    let sent = chats.lock().expect("lock").clone();
    assert_eq!(sent.len(), 2, "{sent:?}");
    for (body, budget) in sent.iter().zip([1024, 4096]) {
        assert_eq!(body["reasoning_budget_tokens"], budget, "{body}");
        assert!(body.get("reasoning_effort").is_none(), "{body}");
    }

    let search = http
        .post(format!("{base}/v1/responses"))
        .bearer_auth(&access)
        .json(&json!({
            "model": "Qwen3.5-9B",
            "input": "rust ownership",
            "tools": [{"type": "web_search"}]
        }))
        .send()
        .await
        .expect("search");
    assert_eq!(search.status(), 200);
    let bytes = search.bytes().await.expect("search body");
    let parsed: async_openai::types::responses::Response =
        serde_json::from_slice(&bytes).expect("client Response");
    assert_eq!(
        parsed.output_text().as_deref(),
        Some("Ownership is a Rust rule.")
    );
    let rendered = serde_json::to_value(&parsed).expect("render");
    assert!(
        rendered.to_string().contains("https://www.rust-lang.org/"),
        "{rendered}"
    );
    assert!(parsed.usage.is_none(), "{rendered}");
}

fn ensure_db() {
    let listed = std::process::Command::new("psql")
        .args([
            "-h",
            "127.0.0.1",
            "-d",
            "postgres",
            "-At",
            "-c",
            "SELECT 1 FROM pg_database WHERE datname = 'cortex_api_handled'",
        ])
        .output()
        .expect("psql");
    if !String::from_utf8_lossy(&listed.stdout).contains('1') {
        let created = std::process::Command::new("psql")
            .args([
                "-h",
                "127.0.0.1",
                "-d",
                "postgres",
                "-c",
                "CREATE DATABASE cortex_api_handled",
            ])
            .status()
            .expect("createdb");
        assert!(created.success(), "create cortex_api_handled");
    }
}

fn truncate() {
    let status = std::process::Command::new("psql")
        .args([
            "-h",
            "127.0.0.1",
            "-d",
            DB,
            "-c",
            "TRUNCATE users, browser_sessions, auth_transactions, refresh_tokens, device_grants, login_attempts, rate_buckets CASCADE",
        ])
        .status();
    if let Ok(status) = status {
        let _ = status.success();
    }
}

async fn sign_in(http: &reqwest::Client, base: &str) -> String {
    let (verifier, challenge) = pkce();
    let redirect = "http://127.0.0.1:1/callback";
    let authorize = format!(
        "{base}/authorize?response_type=code&client_id={}&redirect_uri={}&scope=openid&code_challenge={}&code_challenge_method=S256&state=handled&nonce=handled",
        cortex_api::CLIENT_ID,
        urlencoding(redirect),
        urlencoding(&challenge),
    );
    let next = authorize.trim_start_matches(base).to_owned();
    http.get(&authorize).send().await.expect("authorize");
    let created = http
        .post(format!("{base}/register"))
        .form(&[
            ("email", "handled@example.com"),
            ("password", "correct-horse"),
            ("first_name", "A"),
            ("last_name", "B"),
            ("next", next.as_str()),
        ])
        .send()
        .await
        .expect("register");
    assert_eq!(created.status(), 303);
    let allow = http
        .post(format!("{base}/consent"))
        .form(&[("state", "handled"), ("decision", "allow")])
        .send()
        .await
        .expect("allow");
    let loc = allow
        .headers()
        .get(reqwest::header::LOCATION)
        .expect("location")
        .to_str()
        .expect("str")
        .to_owned();
    let code = url::form_urlencoded::parse(loc.split_once('?').expect("query").1.as_bytes())
        .find(|(name, _)| name == "code")
        .map(|(_, value)| value.into_owned())
        .expect("code");
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
    tokens["access_token"]
        .as_str()
        .expect("access")
        .to_owned()
}

fn pkce() -> (String, String) {
    let verifier = "handled-verifier-handled-verifier-handled-verifier";
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(digest);
    (verifier.to_owned(), challenge)
}

fn urlencoding(text: &str) -> String {
    url::form_urlencoded::byte_serialize(text.as_bytes()).collect()
}

async fn mock_llama(chats: Arc<Mutex<Vec<Value>>>) -> String {
    let chats_route = chats.clone();
    let app = Router::new()
        .route(
            "/props",
            get(|| async { Json(json!({"default_generation_settings": {"n_ctx": 4096}})) }),
        )
        .route(
            "/v1/chat/completions",
            post(move |body: Bytes| {
                let chats_route = chats_route.clone();
                async move {
                    let payload: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
                    chats_route.lock().expect("lock").push(payload);
                    Json(json!({
                        "id": "chatcmpl-1",
                        "object": "chat.completion",
                        "choices": [{
                            "index": 0,
                            "message": {"role": "assistant", "content": "ok"},
                            "finish_reason": "stop"
                        }]
                    }))
                }
            }),
        )
        .route("/v1/responses", post(mock_responses));
    bind(app).await
}

async fn mock_responses(body: Bytes) -> impl IntoResponse {
    let text = String::from_utf8_lossy(&body);
    if text.contains("function_call_output") {
        Json(json!({
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Ownership is a Rust rule."}]
            }]
        }))
    } else {
        Json(json!({
            "output": [{
                "type": "function_call",
                "name": "web_search",
                "call_id": "call_1",
                "arguments": "{\"query\":\"rust ownership\"}"
            }]
        }))
    }
}

async fn mock_brave() -> String {
    let app = Router::new().route(
        "/web/search",
        get(|headers: HeaderMap| async move {
            let token = headers
                .get("x-subscription-token")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("");
            if token != BRAVE {
                return StatusCode::UNAUTHORIZED.into_response();
            }
            Json(json!({
                "web": {"results": [{
                    "title": "Rust",
                    "url": "https://www.rust-lang.org/",
                    "description": "A language"
                }]}
            }))
            .into_response()
        }),
    );
    bind(app).await
}

async fn bind(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock");
    });
    format!("http://{addr}")
}
