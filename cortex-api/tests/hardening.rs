// Test clients talk to loopback mocks only.
#![allow(clippy::disallowed_methods)]
//! Production limits: per-address and per-user rate, daily quotas, the inference
//! queue and breaker, runtime key rotation, retention, versioned migrations,
//! and the shared Brave bucket.
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cortex_api::jwt::b64url;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn db_url(name: &str) -> String {
    format!("postgres://127.0.0.1/{name}")
}

struct Api {
    base: String,
    http: reqwest::Client,
    server: cortex_api::Running,
    db: String,
}

async fn start(db: &str, tune: impl FnOnce(&mut cortex_api::Config)) -> Api {
    psql(
        db,
        "TRUNCATE users, browser_sessions, auth_transactions, refresh_tokens, device_grants, login_attempts, rate_buckets CASCADE",
    );
    psql(db, "DELETE FROM upstream_buckets");
    psql(db, "DELETE FROM signing_keys");
    let mut config = cortex_api::Config::new("127.0.0.1:0".parse().expect("addr"), db_url(db));
    config.login_limit = 100;
    tune(&mut config);
    let server = cortex_api::serve(config).await.expect("serve");
    let base = format!("http://{}", server.addr);
    Api {
        base,
        http: client(),
        server,
        db: db.to_owned(),
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .cookie_store(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .expect("client")
}

fn psql(db: &str, sql: &str) -> String {
    let out = std::process::Command::new("psql")
        .args(["-h", "127.0.0.1", "-d", db, "-At", "-c", sql])
        .output()
        .expect("psql");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

async fn sign_in(api: &Api, email: &str) -> String {
    let http = client();
    let base = &api.base;
    let (verifier, challenge) = pkce();
    let state = format!("state-{email}");
    let redirect = "http://127.0.0.1:1/callback";
    let authorize = format!(
        "{base}/authorize?response_type=code&client_id={}&redirect_uri={}&scope=openid&code_challenge={}&code_challenge_method=S256&state={state}&nonce={state}",
        cortex_api::CLIENT_ID,
        urlencoding(redirect),
        urlencoding(&challenge),
    );
    let next = authorize.trim_start_matches(base.as_str()).to_owned();
    http.get(&authorize).send().await.expect("authorize");
    let created = http
        .post(format!("{base}/register"))
        .form(&[
            ("email", email),
            ("password", "correct-horse"),
            ("first_name", "A"),
            ("last_name", "B"),
            ("next", next.as_str()),
        ])
        .send()
        .await
        .expect("register");
    assert_eq!(created.status(), 303, "{email}");
    let allow = http
        .post(format!("{base}/consent"))
        .form(&[("state", state.as_str()), ("decision", "allow")])
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
    tokens["access_token"].as_str().expect("access").to_owned()
}

fn pkce() -> (String, String) {
    let mut raw = [0u8; 32];
    rand::Rng::fill(&mut rand::rng(), &mut raw);
    let verifier = URL_SAFE_NO_PAD.encode(raw);
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn urlencoding(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
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

#[derive(Clone)]
struct Llama {
    calls: Arc<AtomicUsize>,
    delay: Duration,
    status: StatusCode,
}

async fn mock_llama(delay: Duration, status: StatusCode) -> (String, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let state = Llama {
        calls: calls.clone(),
        delay,
        status,
    };
    let app = Router::new()
        .route(
            "/props",
            get(|| async {
                Json(json!({"default_generation_settings": {"n_ctx": 4096}, "total_slots": 1}))
            }),
        )
        .route("/v1/chat/completions", post(chat))
        .with_state(state);
    (bind(app).await, calls)
}

async fn chat(State(llama): State<Llama>) -> Response {
    llama.calls.fetch_add(1, Ordering::Relaxed);
    tokio::time::sleep(llama.delay).await;
    if !llama.status.is_success() {
        return llama.status.into_response();
    }
    Json(json!({"id": "x", "choices": [{"message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}]})).into_response()
}

async fn post_chat(api: &Api, token: &str) -> reqwest::Response {
    api.http
        .post(format!("{}/v1/chat/completions", api.base))
        .bearer_auth(token)
        .json(&json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .expect("chat")
}

#[tokio::test]
async fn migrations_are_versioned_and_rerun_clean() {
    let api = start("cortex_api_hard_migrate", |_| {}).await;
    assert_eq!(
        psql(
            &api.db,
            "SELECT string_agg(version::text, ',' ORDER BY version) FROM schema_migrations"
        ),
        "1,2,3"
    );
    api.server.shutdown().await.expect("shutdown");
    let again = start("cortex_api_hard_migrate", |_| {}).await;
    assert_eq!(
        psql(&again.db, "SELECT count(*) FROM schema_migrations"),
        "3",
        "a restart applies nothing twice"
    );
}

#[tokio::test]
async fn address_limit_follows_the_forwarded_client_not_the_proxy() {
    let api = start("cortex_api_hard_ip", |config| config.ip_rate_per_min = 3).await;
    let url = format!("{}/v1/login-config", api.base);
    for _ in 0..3 {
        let ok = api
            .http
            .get(&url)
            .header("x-forwarded-for", "198.51.100.7")
            .send()
            .await
            .expect("get");
        assert_eq!(ok.status(), 200);
    }
    let blocked = api
        .http
        .get(&url)
        .header("x-forwarded-for", "198.51.100.7")
        .send()
        .await
        .expect("get");
    assert_eq!(blocked.status(), 429);
    assert!(blocked.headers().get("retry-after").is_some());
    let other = api
        .http
        .get(&url)
        .header("x-forwarded-for", "203.0.113.9")
        .send()
        .await
        .expect("get");
    assert_eq!(
        other.status(),
        200,
        "another client behind the same proxy is unaffected"
    );
    let health = api
        .http
        .get(format!("{}/healthz", api.base))
        .send()
        .await
        .expect("health");
    assert_eq!(health.status(), 200, "health checks are not counted");
}

#[tokio::test]
async fn user_rate_limit_is_per_user() {
    let api = start("cortex_api_hard_user", |config| {
        config.user_rate_per_min = 3
    })
    .await;
    let a = sign_in(&api, "a@example.com").await;
    let b = sign_in(&api, "b@example.com").await;
    let url = format!("{}/v1/user", api.base);
    for _ in 0..3 {
        assert_eq!(
            api.http
                .get(&url)
                .bearer_auth(&a)
                .send()
                .await
                .expect("get")
                .status(),
            200
        );
    }
    let blocked = api
        .http
        .get(&url)
        .bearer_auth(&a)
        .send()
        .await
        .expect("get");
    assert_eq!(blocked.status(), 429);
    assert!(blocked.headers().get("retry-after").is_some());
    assert_eq!(
        api.http
            .get(&url)
            .bearer_auth(&b)
            .send()
            .await
            .expect("get")
            .status(),
        200
    );
}

#[tokio::test]
async fn daily_inference_quota_stops_one_user_only() {
    let (llama, calls) = mock_llama(Duration::ZERO, StatusCode::OK).await;
    let api = start("cortex_api_hard_quota", |config| {
        config.llama_url = Some(llama);
        config.daily_inference_quota = 2;
    })
    .await;
    let a = sign_in(&api, "a@example.com").await;
    let b = sign_in(&api, "b@example.com").await;
    assert_eq!(post_chat(&api, &a).await.status(), 200);
    assert_eq!(post_chat(&api, &a).await.status(), 200);
    let over = post_chat(&api, &a).await;
    assert_eq!(over.status(), 429);
    assert!(over.headers().get("retry-after").is_some());
    assert_eq!(
        post_chat(&api, &b).await.status(),
        200,
        "the quota is per user"
    );
    assert_eq!(
        calls.load(Ordering::Relaxed),
        3,
        "the refused request never reached llama-server"
    );
}

#[tokio::test]
async fn inference_queue_waits_then_sheds() {
    let (llama, _calls) = mock_llama(Duration::from_millis(700), StatusCode::OK).await;
    let api = Arc::new(
        start("cortex_api_hard_queue", |config| {
            config.llama_url = Some(llama);
            config.parallel = 1;
            config.queue_max = 1;
            config.queue_wait_secs = 10;
        })
        .await,
    );
    let token = sign_in(&api, "a@example.com").await;
    let mut tasks = Vec::new();
    for _ in 0..3 {
        let api = api.clone();
        let token = token.clone();
        tasks.push(tokio::spawn(async move {
            let response = post_chat(&api, &token).await;
            (
                response.status().as_u16(),
                response.headers().get("retry-after").is_some(),
            )
        }));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut statuses = Vec::new();
    for task in tasks {
        statuses.push(task.await.expect("task"));
    }
    statuses.sort();
    assert_eq!(
        statuses.iter().filter(|(status, _)| *status == 200).count(),
        2,
        "one runs, one waits and then runs: {statuses:?}"
    );
    let shed: Vec<_> = statuses
        .iter()
        .filter(|(status, _)| *status == 429)
        .collect();
    assert_eq!(
        shed.len(),
        1,
        "the third finds the queue full: {statuses:?}"
    );
    assert!(shed[0].1, "429 carries Retry-After");
}

#[tokio::test]
async fn failing_llama_opens_the_breaker() {
    let (llama, calls) = mock_llama(Duration::ZERO, StatusCode::INTERNAL_SERVER_ERROR).await;
    let api = start("cortex_api_hard_breaker", |config| {
        config.llama_url = Some(llama)
    })
    .await;
    let token = sign_in(&api, "a@example.com").await;
    for _ in 0..5 {
        assert_eq!(post_chat(&api, &token).await.status(), 500);
    }
    let open = post_chat(&api, &token).await;
    assert_eq!(open.status(), 503);
    assert!(open.headers().get("retry-after").is_some());
    assert_eq!(
        calls.load(Ordering::Relaxed),
        5,
        "an open breaker sends nothing upstream"
    );
    let metrics = api
        .http
        .get(format!("{}/metrics", api.base))
        .send()
        .await
        .expect("metrics")
        .text()
        .await
        .expect("text");
    assert!(
        metrics.contains("cortex_api_llama_breaker_open 1"),
        "{metrics}"
    );
}

#[tokio::test]
async fn signing_key_rotates_while_running_and_old_tokens_keep_verifying() {
    let api = start("cortex_api_hard_keys", |config| config.key_reload_secs = 1).await;
    let token = sign_in(&api, "a@example.com").await;
    let kids = |jwks: &Value| jwks["keys"].as_array().expect("keys").len();
    let before: Value = api
        .http
        .get(format!("{}/oauth2/jwks", api.base))
        .send()
        .await
        .expect("jwks")
        .json()
        .await
        .expect("json");
    assert_eq!(kids(&before), 1);
    psql(
        &api.db,
        "UPDATE signing_keys SET created_at = now() - interval '40 days' WHERE retired_at IS NULL",
    );
    let mut after = before.clone();
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        after = api
            .http
            .get(format!("{}/oauth2/jwks", api.base))
            .send()
            .await
            .expect("jwks")
            .json()
            .await
            .expect("json");
        if kids(&after) == 2 {
            break;
        }
    }
    assert_eq!(
        kids(&after),
        2,
        "the rotated key is published next to the retiring one"
    );
    let user = api
        .http
        .get(format!("{}/v1/user", api.base))
        .bearer_auth(&token)
        .send()
        .await
        .expect("user");
    assert_eq!(
        user.status(),
        200,
        "a token signed by the retired key still verifies in the grace window"
    );
    let fresh = sign_in(&api, "b@example.com").await;
    let header = fresh.split('.').next().expect("header");
    let header: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(header).expect("b64")).expect("json");
    let old_header: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(token.split('.').next().expect("h"))
            .expect("b64"),
    )
    .expect("json");
    assert_ne!(
        header["kid"], old_header["kid"],
        "new tokens use the new key"
    );
}

#[tokio::test]
async fn telemetry_is_kept_per_user_and_purged_after_retention() {
    let api = start("cortex_api_hard_retention", |config| {
        config.telemetry_retention_days = 30;
        config.purge_secs = 1;
    })
    .await;
    let a = sign_in(&api, "a@example.com").await;
    let b = sign_in(&api, "b@example.com").await;
    for (token, body) in [(&a, "from-a"), (&b, "from-b")] {
        let sent = api
            .http
            .post(format!("{}/v1/traces", api.base))
            .bearer_auth(token)
            .body(body)
            .send()
            .await
            .expect("traces");
        assert_eq!(sent.status(), 200);
    }
    assert_eq!(
        psql(
            &api.db,
            "SELECT count(DISTINCT user_id) FROM telemetry_events WHERE kind = 'traces'"
        ),
        "2",
        "each payload belongs to its sender"
    );
    psql(
        &api.db,
        "UPDATE telemetry_events SET created_at = now() - interval '45 days' WHERE payload = 'from-a'::bytea",
    );
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if psql(
            &api.db,
            "SELECT count(*) FROM telemetry_events WHERE payload = 'from-a'::bytea",
        ) == "0"
        {
            break;
        }
    }
    assert_eq!(
        psql(
            &api.db,
            "SELECT count(*) FROM telemetry_events WHERE payload = 'from-a'::bytea"
        ),
        "0"
    );
    assert_eq!(
        psql(
            &api.db,
            "SELECT count(*) FROM telemetry_events WHERE payload = 'from-b'::bytea"
        ),
        "1"
    );
}

#[derive(Clone)]
struct Brave {
    calls: Arc<AtomicUsize>,
}

async fn brave_search(State(brave): State<Brave>, _headers: HeaderMap) -> Response {
    brave.calls.fetch_add(1, Ordering::Relaxed);
    (
        [("x-ratelimit-remaining", "0, 4000"), ("x-ratelimit-reset", "30, 86400")],
        Json(json!({"web": {"results": [{"title": "Rust", "url": "https://www.rust-lang.org/", "description": "d"}]}})),
    )
        .into_response()
}

async fn search_llama() -> String {
    let counter = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/props", get(|| async { Json(json!({"default_generation_settings": {"n_ctx": 4096}})) }))
        .route(
            "/v1/responses",
            post(move |body: axum::body::Bytes| {
                let counter = counter.clone();
                async move {
                    if String::from_utf8_lossy(&body).contains("function_call_output") {
                        Json(json!({"output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "answer"}]}]}))
                    } else {
                        let n = counter.fetch_add(1, Ordering::Relaxed);
                        let arguments = json!({"query": format!("rust {n}")}).to_string();
                        Json(json!({"output": [{"type": "function_call", "name": "web_search", "call_id": "c1", "arguments": arguments}]}))
                    }
                }
            }),
        );
    bind(app).await
}

#[tokio::test]
async fn brave_headers_drive_a_shared_bucket_and_search_has_a_daily_quota() {
    let calls = Arc::new(AtomicUsize::new(0));
    let brave = bind(
        Router::new()
            .route("/web/search", get(brave_search))
            .with_state(Brave {
                calls: calls.clone(),
            }),
    )
    .await;
    let llama = search_llama().await;
    let api = start("cortex_api_hard_brave", |config| {
        config.llama_url = Some(llama);
        config.brave_token = Some("token".to_owned());
        config.brave_base = brave;
        config.daily_search_quota = 1000;
    })
    .await;
    let token = sign_in(&api, "a@example.com").await;
    let ask = || async {
        api.http
            .post(format!("{}/v1/responses", api.base))
            .bearer_auth(&token)
            .json(
                &json!({"model": "m", "input": "what is rust", "tools": [{"type": "web_search"}]}),
            )
            .send()
            .await
            .expect("responses")
    };
    assert_eq!(ask().await.status(), 200);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        psql(
            &api.db,
            "SELECT remaining FROM upstream_buckets WHERE name = 'brave'"
        ),
        "0"
    );
    let waiting = ask().await;
    assert_eq!(
        waiting.status(),
        429,
        "Brave said nothing is left in the window"
    );
    let retry: u64 = waiting
        .headers()
        .get("retry-after")
        .expect("retry-after")
        .to_str()
        .expect("str")
        .parse()
        .expect("secs");
    assert!((1..=30).contains(&retry), "{retry}");
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "the bucket stopped the second call before Brave"
    );
    assert_eq!(
        psql(
            &api.db,
            "SELECT count FROM usage_daily WHERE kind = 'search'"
        ),
        "2"
    );

    psql(
        &api.db,
        "UPDATE upstream_buckets SET reset_at = now() - interval '1 second'",
    );
    psql(
        &api.db,
        "UPDATE usage_daily SET count = 1000 WHERE kind = 'search'",
    );
    let over = ask().await;
    assert_eq!(over.status(), 429, "the daily search quota is spent");
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn context_window_follows_llama_server_once_it_is_up() {
    // llama-server is still loading when cortex-api starts: /props fails twice.
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let llama = bind(Router::new().route(
        "/props",
        get(move || {
            let seen = seen.clone();
            async move {
                if seen.fetch_add(1, Ordering::Relaxed) < 2 {
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                Json(json!({"default_generation_settings": {"n_ctx": 12345}, "total_slots": 1}))
                    .into_response()
            }
        }),
    ))
    .await;
    let api = start("cortex_api_hard_props", |config| {
        config.llama_url = Some(llama);
        config.context_window = 777;
        config.context_percent = 100;
        config.props_refresh_secs = 1;
    })
    .await;
    let token = sign_in(&api, "a@example.com").await;
    let window = |models: Value| {
        models["data"][0]["context_window"]
            .as_u64()
            .expect("window")
    };
    let fetch = || async {
        let models: Value = api
            .http
            .get(format!("{}/v1/models", api.base))
            .bearer_auth(&token)
            .send()
            .await
            .expect("models")
            .json()
            .await
            .expect("json");
        window(models)
    };
    let mut last = fetch().await;
    for _ in 0..20 {
        if last == 12345 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        last = fetch().await;
    }
    assert_eq!(
        last, 12345,
        "the advertised window tracks llama-server after it comes up"
    );
}

#[tokio::test]
async fn without_a_search_token_the_hosted_tool_is_dropped_and_the_turn_still_runs() {
    let seen = Arc::new(std::sync::Mutex::new(String::new()));
    let sink = seen.clone();
    let llama = bind(
        Router::new()
            .route("/props", get(|| async { Json(json!({"default_generation_settings": {"n_ctx": 4096}})) }))
            .route(
                "/v1/responses",
                post(move |body: axum::body::Bytes| {
                    let sink = sink.clone();
                    async move {
                        *sink.lock().expect("lock") = String::from_utf8_lossy(&body).into_owned();
                        Json(json!({"output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "hi"}]}]}))
                    }
                }),
            ),
    )
    .await;
    let api = start("cortex_api_hard_nosearch", |config| {
        config.llama_url = Some(llama)
    })
    .await;
    let token = sign_in(&api, "a@example.com").await;
    let models: Value = api
        .http
        .get(format!("{}/v1/models", api.base))
        .bearer_auth(&token)
        .send()
        .await
        .expect("models")
        .json()
        .await
        .expect("json");
    assert_eq!(models["data"][0]["supports_backend_search"], false);
    let settings: Value = api
        .http
        .get(format!("{}/v1/settings", api.base))
        .bearer_auth(&token)
        .send()
        .await
        .expect("settings")
        .json()
        .await
        .expect("json");
    assert!(
        settings.get("web_search_model").is_none(),
        "search is advertised only when it can run"
    );
    let answer = api
        .http
        .post(format!("{}/v1/responses", api.base))
        .bearer_auth(&token)
        .json(&json!({"model": "m", "input": "hello", "tools": [
            {"type": "function", "name": "read_file", "parameters": {"type": "object"}},
            {"type": "web_search"}
        ]}))
        .send()
        .await
        .expect("responses");
    assert_eq!(answer.status(), 200);
    let forwarded = seen.lock().expect("lock").clone();
    assert!(forwarded.contains("read_file"), "{forwarded}");
    assert!(
        !forwarded.contains("web_search"),
        "the hosted search tool never reaches llama-server: {forwarded}"
    );
}
