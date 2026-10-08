// Test clients talk to loopback mocks only.
#![allow(clippy::disallowed_methods)]
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use cortex_api::jwt::b64url;
use prod_mc_cli_chat_proxy_types::{
    FeedbackRequestUpdateResponse, FeedbackResponse, SessionSignalsUpdateResponse, SubagentBundle,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

const DB: &str = "postgres://127.0.0.1/cortex_api_test";

struct Session {
    base: String,
    http: reqwest::Client,
    server: cortex_api::Running,
}

impl Session {
    async fn start() -> Self {
        truncate();
        let mut config =
            cortex_api::Config::new("127.0.0.1:0".parse().expect("addr"), DB.to_owned());
        config.register_limit = 2;
        config.login_limit = 30;
        config.lock_after = 3;
        config.lock_secs = 900;
        let running = cortex_api::serve(config).await.expect("serve");
        let base = format!("http://{}", running.addr);
        Self {
            base,
            http: http_client(),
            server: running,
        }
    }
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .cookie_store(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .expect("client")
}

fn scalar(sql: &str) -> String {
    let out = std::process::Command::new("psql")
        .args([
            "-h",
            "127.0.0.1",
            "-d",
            "cortex_api_test",
            "-At",
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            sql,
        ])
        .output()
        .expect("psql");
    assert!(
        out.status.success(),
        "psql: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn truncate() {
    let status = std::process::Command::new("psql")
        .args([
            "-h",
            "127.0.0.1",
            "-d",
            "cortex_api_test",
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            "TRUNCATE users, browser_sessions, auth_transactions, refresh_tokens, device_grants, login_attempts, rate_buckets CASCADE",
        ])
        .status();
    if let Ok(status) = status {
        let _ = status.success();
    }
}

fn pkce() -> (String, String) {
    let mut raw = [0u8; 32];
    rand_bytes(&mut raw);
    let verifier = URL_SAFE_NO_PAD.encode(raw);
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn rand_bytes(buf: &mut [u8]) {
    let mut rng = rand::rng();
    rand::Rng::fill(&mut rng, buf);
}

#[tokio::test]
async fn phase2_contract_matches_proxy_types() {
    let mut session = Session::start().await;
    let base = session.base.clone();

    for i in 0..2 {
        let response = session
            .http
            .post(format!("{base}/register"))
            .form(&[
                ("email", format!("rate{i}@example.com")),
                ("password", "correct-horse".to_owned()),
                ("first_name", "Rate".to_owned()),
                ("last_name", "Limit".to_owned()),
                ("next", "/authorize".to_owned()),
            ])
            .send()
            .await
            .expect("register");
        assert_eq!(response.status(), 303, "register {i}");
    }
    let limited = session
        .http
        .post(format!("{base}/register"))
        .form(&[
            ("email", "rate2@example.com"),
            ("password", "correct-horse"),
            ("first_name", "Rate"),
            ("last_name", "Limit"),
            ("next", "/authorize"),
        ])
        .send()
        .await
        .expect("limited register");
    assert_eq!(limited.status(), 429);
    let retry = limited
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .expect("retry after");
    assert!(retry.parse::<u64>().is_ok());

    truncate();
    session.http = http_client();

    let discovery: Value = session
        .http
        .get(format!("{base}/.well-known/openid-configuration"))
        .send()
        .await
        .expect("discovery")
        .json()
        .await
        .expect("discovery json");
    assert_eq!(discovery["issuer"], "https://llm.imabee.com");
    assert_eq!(
        discovery["authorization_endpoint"],
        "https://llm.imabee.com/authorize"
    );
    assert_eq!(
        discovery["token_endpoint"],
        "https://llm.imabee.com/oauth2/token"
    );
    assert_eq!(discovery["jwks_uri"], "https://llm.imabee.com/oauth2/jwks");
    assert_eq!(
        discovery["id_token_signing_alg_values_supported"][0],
        "RS256"
    );

    let health: Value = session
        .http
        .get(format!("{base}/healthz"))
        .send()
        .await
        .expect("health")
        .json()
        .await
        .expect("health json");
    assert_eq!(health["status"], "ok");
    let ready = session
        .http
        .get(format!("{base}/readyz"))
        .send()
        .await
        .expect("ready");
    assert_eq!(ready.status(), 200);

    let stable = session
        .http
        .get(format!("{base}/cli/stable"))
        .send()
        .await
        .expect("stable")
        .text()
        .await
        .expect("stable text");
    assert_eq!(stable.trim(), "0.0.0");

    let login_config: Value = session
        .http
        .get(format!("{base}/v1/login-config"))
        .send()
        .await
        .expect("login config")
        .json()
        .await
        .expect("login config json");
    assert!(login_config.get("device_flow").is_none() || login_config["device_flow"].is_null());

    let unauth = session
        .http
        .get(format!("{base}/v1/user"))
        .send()
        .await
        .expect("user");
    assert_eq!(unauth.status(), 401);

    let (_verifier, challenge) = pkce();
    let state = "state-1";
    let nonce = "nonce-1";
    let redirect = "http://127.0.0.1:1/callback";
    let authorize = format!(
        "{base}/authorize?response_type=code&client_id={client}&redirect_uri={redirect}&scope={scope}&code_challenge={challenge}&code_challenge_method=S256&state={state}&nonce={nonce}",
        client = cortex_api::CLIENT_ID,
        scope = "openid profile email offline_access cortex-cli:access api:access",
        redirect = urlencoding(redirect),
        challenge = urlencoding(&challenge),
    );
    let page = session
        .http
        .get(&authorize)
        .send()
        .await
        .expect("authorize");
    assert_eq!(page.status(), 200);
    let html = page.text().await.expect("authorize html");
    assert!(html.contains("Sign in"));
    assert!(html.contains("action=\"/login\""));

    let next = authorize.trim_start_matches(base.as_str());
    let signed_in = session
        .http
        .post(format!("{base}/login"))
        .form(&[
            ("email", "ada@example.com"),
            ("password", "correct-horse"),
            ("next", next),
        ])
        .send()
        .await
        .expect("login");
    assert_eq!(signed_in.status(), 401, "account does not exist yet");

    let created = session
        .http
        .post(format!("{base}/register"))
        .form(&[
            ("email", "ada@example.com"),
            ("password", "correct-horse"),
            ("first_name", "Ada"),
            ("last_name", "Lovelace"),
            ("next", next),
        ])
        .send()
        .await
        .expect("create");
    assert_eq!(created.status(), 303);
    let consent_url = created
        .headers()
        .get(reqwest::header::LOCATION)
        .expect("location")
        .to_str()
        .expect("location str")
        .to_owned();
    let consent = session
        .http
        .get(format!("{base}{consent_url}"))
        .send()
        .await
        .expect("consent");
    let consent_html = consent.text().await.expect("consent html");
    assert!(consent_html.contains("Allow"));
    assert!(consent_html.contains("Deny"));

    let denied = session
        .http
        .post(format!("{base}/consent"))
        .form(&[("state", "missing"), ("decision", "deny")])
        .send()
        .await
        .expect("missing consent");
    assert_eq!(denied.status(), 400);

    let allow = session
        .http
        .post(format!("{base}/consent"))
        .form(&[("state", state), ("decision", "allow")])
        .send()
        .await
        .expect("allow");
    assert_eq!(allow.status(), 303);
    let loc = allow
        .headers()
        .get(reqwest::header::LOCATION)
        .expect("redirect")
        .to_str()
        .expect("redirect str");
    assert!(loc.starts_with("http://127.0.0.1:1/callback?"));
    let code = query_param(loc, "code").expect("code");
    assert_eq!(query_param(loc, "state").as_deref(), Some(state));

    let bad = session
        .http
        .post(format!("{base}/oauth2/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", redirect),
            ("client_id", cortex_api::CLIENT_ID),
            (
                "code_verifier",
                "not-the-verifier-not-the-verifier-not-the-verifier",
            ),
        ])
        .send()
        .await
        .expect("bad pkce");
    assert_eq!(bad.status(), 400);
    let bad_body: Value = bad.json().await.expect("bad json");
    assert_eq!(bad_body["error"], "invalid_grant");

    let (verifier, challenge) = pkce();
    let state = "state-2";
    let nonce = "nonce-2";
    let authorize = format!(
        "{base}/authorize?response_type=code&client_id={client}&redirect_uri={redirect}&scope={scope}&code_challenge={challenge}&code_challenge_method=S256&state={state}&nonce={nonce}",
        client = cortex_api::CLIENT_ID,
        scope = "openid profile email",
        redirect = urlencoding(redirect),
        challenge = urlencoding(&challenge),
    );
    session
        .http
        .get(&authorize)
        .send()
        .await
        .expect("authorize 2")
        .error_for_status()
        .expect("authorize status");
    let allow = session
        .http
        .post(format!("{base}/consent"))
        .form(&[("state", state), ("decision", "allow")])
        .send()
        .await
        .expect("allow 2");
    let loc = allow
        .headers()
        .get(reqwest::header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let code = query_param(&loc, "code").unwrap();
    let tokens = session
        .http
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
        .expect("token");
    assert_eq!(tokens.status(), 200);
    let tokens: Value = tokens.json().await.expect("tokens");
    let access = tokens["access_token"].as_str().expect("access").to_owned();
    let refresh = tokens["refresh_token"]
        .as_str()
        .expect("refresh")
        .to_owned();
    let id_token = tokens["id_token"].as_str().expect("id").to_owned();
    assert_signed(&session, &id_token, nonce).await;
    assert_signed(&session, &access, "").await;

    let reused_code = session
        .http
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
        .expect("reuse code");
    assert_eq!(reused_code.status(), 400);

    let refreshed = session
        .http
        .post(format!("{base}/oauth2/token"))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh.as_str()),
            ("client_id", cortex_api::CLIENT_ID),
        ])
        .send()
        .await
        .expect("refresh");
    assert_eq!(refreshed.status(), 200);
    let refreshed: Value = refreshed.json().await.expect("refresh json");
    let refresh_2 = refreshed["refresh_token"].as_str().unwrap().to_owned();
    let access_2 = refreshed["access_token"].as_str().unwrap().to_owned();

    let reuse = session
        .http
        .post(format!("{base}/oauth2/token"))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh.as_str()),
            ("client_id", cortex_api::CLIENT_ID),
        ])
        .send()
        .await
        .expect("reuse");
    let reuse_body: Value = reuse.json().await.expect("reuse json");
    assert_eq!(reuse_body["error"], "invalid_grant");
    let family = session
        .http
        .post(format!("{base}/oauth2/token"))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_2.as_str()),
            ("client_id", cortex_api::CLIENT_ID),
        ])
        .send()
        .await
        .expect("family");
    let family_body: Value = family.json().await.expect("family json");
    assert_eq!(family_body["error"], "invalid_grant");

    let user: Value = session
        .http
        .get(format!("{base}/v1/user?include=subscription"))
        .bearer_auth(&access_2)
        .header("x-cortex-token-auth", cortex_api::TOKEN_HEADER)
        .send()
        .await
        .expect("user")
        .json()
        .await
        .expect("user json");
    assert_eq!(user["email"], "ada@example.com");
    assert_eq!(user["firstName"], "Ada");
    assert_eq!(user["lastName"], "Lovelace");
    assert!(user["userId"].as_str().unwrap().len() > 8);
    assert_eq!(user["subscriptionTier"], "premium");

    let settings: Value = session
        .http
        .get(format!("{base}/v1/settings"))
        .bearer_auth(&access_2)
        .send()
        .await
        .expect("settings")
        .json()
        .await
        .expect("settings json");
    assert_eq!(settings["allow_access"], true);
    assert_eq!(settings["image_gen_enabled"], false);
    assert_eq!(settings["video_gen_enabled"], false);
    assert_eq!(settings["voice_mode_enabled"], false);
    assert_eq!(settings["oauth2_issuer"], "https://llm.imabee.com");
    assert_eq!(settings["oauth2_client_id"], cortex_api::CLIENT_ID);

    let models: Value = session
        .http
        .get(format!("{base}/v1/models"))
        .bearer_auth(&access_2)
        .send()
        .await
        .expect("models")
        .json()
        .await
        .expect("models json");
    assert_eq!(models["data"][0]["model"], "Qwen3.5-9B");
    assert_eq!(models["data"][0]["context_window"], 39321);
    assert_eq!(models["data"][0]["api_backend"], "chat_completions");
    assert_eq!(
        models["data"][0]["supports_backend_search"], false,
        "no search token, no advertised search"
    );

    let bundle = session
        .http
        .get(format!("{base}/v1/subagents/bundle"))
        .bearer_auth(&access_2)
        .send()
        .await
        .expect("bundle");
    let bundle: SubagentBundle = bundle.json().await.expect("bundle type");
    assert_eq!(bundle.version, "1");
    assert!(bundle.agents.is_empty());

    let archive = session
        .http
        .get(format!("{base}/v1/bundle/archive"))
        .bearer_auth(&access_2)
        .send()
        .await
        .expect("archive")
        .bytes()
        .await
        .expect("archive bytes");
    assert_eq!(&archive[..2], &[0x1f, 0x8b]);
    let decoder = flate2::read::GzDecoder::new(&archive[..]);
    let mut tar = tar::Archive::new(decoder);
    let mut saw_version = false;
    for entry in tar.entries().expect("tar") {
        let mut entry = entry.expect("entry");
        if entry.path().ok().as_ref().map(|p| p.as_os_str())
            == Some(std::ffi::OsStr::new("bundle.json"))
        {
            let mut text = String::new();
            std::io::Read::read_to_string(&mut entry, &mut text).expect("read");
            let meta: Value = serde_json::from_str(&text).expect("bundle json");
            assert_eq!(meta["version"], "1");
            saw_version = true;
        }
    }
    assert!(saw_version);

    let deployment = session
        .http
        .get(format!("{base}/v1/deployment/config"))
        .bearer_auth(&access_2)
        .header("x-cortex-managed-config-nonce", "nonce-echo")
        .send()
        .await
        .expect("deployment");
    assert_eq!(deployment.status(), 200);
    assert_eq!(
        deployment
            .headers()
            .get("x-cortex-managed-config-nonce")
            .and_then(|v| v.to_str().ok()),
        Some("nonce-echo")
    );
    let deployment: Value = deployment.json().await.expect("deployment json");
    assert_eq!(deployment, serde_json::json!({}));

    let privacy = session
        .http
        .put(format!("{base}/v1/privacy/coding-data-retention"))
        .bearer_auth(&access_2)
        .json(&serde_json::json!({"codingDataRetentionOptOut": true}))
        .send()
        .await
        .expect("privacy");
    assert_eq!(privacy.status(), 200);

    let feedback = session
        .http
        .post(format!("{base}/v1/feedback"))
        .bearer_auth(&access_2)
        .json(&serde_json::json!({
            "sessionId": "session-1",
            "clientType": "agent",
            "feedbackType": "rating",
            "ratingType": "thumbs",
            "ratingValue": 1
        }))
        .send()
        .await
        .expect("feedback");
    assert_eq!(feedback.status(), 200);
    let feedback: FeedbackResponse = feedback.json().await.expect("feedback type");
    assert!(!feedback.feedback_id.is_empty());

    let complete = session
        .http
        .post(format!("{base}/v1/feedback/requests/req-1/complete"))
        .bearer_auth(&access_2)
        .json(&serde_json::json!({
            "sessionId": "session-1",
            "clientType": "agent",
            "feedbackType": "text"
        }))
        .send()
        .await
        .expect("complete");
    assert_eq!(complete.status(), 200);

    let dismiss = session
        .http
        .post(format!("{base}/v1/feedback/requests/req-2/dismiss"))
        .bearer_auth(&access_2)
        .body("{}")
        .send()
        .await
        .expect("dismiss");
    let dismiss: FeedbackRequestUpdateResponse = dismiss.json().await.expect("dismiss type");
    assert_eq!(dismiss.request_id, "req-2");
    assert_eq!(dismiss.status.to_string(), "dismissed");

    let signals = session
        .http
        .post(format!("{base}/v1/sessions/session-1/signals"))
        .bearer_auth(&access_2)
        .json(&serde_json::json!({"clientType": "agent"}))
        .send()
        .await
        .expect("signals");
    let signals: SessionSignalsUpdateResponse = signals.json().await.expect("signals type");
    assert_eq!(signals.session_id, "session-1");

    let trace_body = b"otlp-bytes";
    let traces = session
        .http
        .post(format!("{base}/v1/traces"))
        .bearer_auth(&access_2)
        .body(trace_body.as_slice())
        .send()
        .await
        .expect("traces");
    assert_eq!(traces.status(), 200);
    let stored = "SELECT count(*) FROM telemetry_events WHERE kind = 'traces' AND user_id IS NOT NULL AND payload = 'otlp-bytes'::bytea";
    assert_eq!(
        scalar(stored),
        "0",
        "a user who opted out of retention has no trace kept"
    );
    let opt_in = session
        .http
        .put(format!("{base}/v1/privacy/coding-data-retention"))
        .bearer_auth(&access_2)
        .json(&serde_json::json!({"codingDataRetentionOptOut": false}))
        .send()
        .await
        .expect("privacy off");
    assert_eq!(opt_in.status(), 200);
    let traces = session
        .http
        .post(format!("{base}/v1/traces"))
        .bearer_auth(&access_2)
        .body(trace_body.as_slice())
        .send()
        .await
        .expect("traces again");
    assert_eq!(traces.status(), 200);
    assert_eq!(
        scalar(stored),
        "1",
        "the trace payload is stored against the signed-in user"
    );
    assert_eq!(
        scalar(
            "SELECT count(*) FROM telemetry_events WHERE kind = 'feedback' AND user_id IS NOT NULL"
        ),
        "1",
        "feedback is explicit and kept even after an opt-out"
    );

    let device = session
        .http
        .post(format!("{base}/oauth2/device/code"))
        .form(&[
            ("client_id", cortex_api::CLIENT_ID),
            ("scope", "openid profile email"),
            ("referrer", "cortex-build"),
        ])
        .send()
        .await
        .expect("device");
    assert_eq!(device.status(), 200);
    let device: Value = device.json().await.expect("device json");
    let device_code = device["device_code"].as_str().unwrap().to_owned();
    let user_code = device["user_code"].as_str().unwrap().to_owned();
    assert!(
        device["verification_uri"]
            .as_str()
            .unwrap()
            .starts_with("https://llm.imabee.com/device")
    );
    assert!(
        user_code
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    );

    let pending = session
        .http
        .post(format!("{base}/oauth2/token"))
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", device_code.as_str()),
            ("client_id", cortex_api::CLIENT_ID),
        ])
        .send()
        .await
        .expect("pending");
    let pending_body: Value = pending.json().await.expect("pending json");
    assert_eq!(pending_body["error"], "authorization_pending");

    let slowed = session
        .http
        .post(format!("{base}/oauth2/token"))
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", device_code.as_str()),
            ("client_id", cortex_api::CLIENT_ID),
        ])
        .send()
        .await
        .expect("slow");
    let slowed_body: Value = slowed.json().await.expect("slow json");
    assert_eq!(slowed_body["error"], "slow_down");

    let approved = session
        .http
        .post(format!("{base}/device/decide"))
        .form(&[("user_code", user_code.as_str()), ("decision", "approve")])
        .send()
        .await
        .expect("approve device");
    assert_eq!(approved.status(), 200);
    tokio::time::sleep(Duration::from_secs(5)).await;
    let device_tokens = session
        .http
        .post(format!("{base}/oauth2/token"))
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", device_code.as_str()),
            ("client_id", cortex_api::CLIENT_ID),
        ])
        .send()
        .await
        .expect("device token");
    assert_eq!(device_tokens.status(), 200, "device token");
    let device_tokens: Value = device_tokens.json().await.expect("device tokens");
    assert!(device_tokens["access_token"].as_str().is_some());
    assert!(device_tokens["id_token"].as_str().is_some());

    for _ in 0..3 {
        let bad = session
            .http
            .post(format!("{base}/login"))
            .form(&[
                ("email", "ada@example.com"),
                ("password", "wrong-password-value"),
                ("next", "/authorize"),
            ])
            .send()
            .await
            .expect("bad password");
        assert_eq!(bad.status(), 401);
    }
    let locked = session
        .http
        .post(format!("{base}/login"))
        .form(&[
            ("email", "ada@example.com"),
            ("password", "wrong-password-value"),
            ("next", "/authorize"),
        ])
        .send()
        .await
        .expect("locked");
    assert_eq!(locked.status(), 429);
    assert!(locked.headers().get(reqwest::header::RETRY_AFTER).is_some());
    let missing = session
        .http
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(&access_2)
        .json(&serde_json::json!({"model": "Qwen3.5-9B", "messages": []}))
        .send()
        .await
        .expect("inference");
    assert_eq!(missing.status(), 503);
    let tampered = format!("{access_2}x");
    let bad_jwt = session
        .http
        .get(format!("{base}/v1/user"))
        .bearer_auth(&tampered)
        .send()
        .await
        .expect("tampered");
    assert_eq!(bad_jwt.status(), 401);
    let models_v2 = session
        .http
        .get(format!("{base}/v1/models-v2"))
        .bearer_auth(&access_2)
        .send()
        .await
        .expect("models v2");
    assert_eq!(models_v2.status(), 200);
    for path in ["/cli/alpha", "/stable", "/alpha"] {
        let channel = session
            .http
            .get(format!("{base}{path}"))
            .send()
            .await
            .expect(path);
        assert_eq!(channel.status(), 200, "{path}");
    }
    let encoded = STANDARD.encode(br#"[{"event":"ping","properties":{"token":"phase5"}}]"#);
    for path in ["/track", "/engage"] {
        let tracked = session
            .http
            .post(format!("{base}{path}?verbose=1"))
            .form(&[("data", encoded.as_str())])
            .send()
            .await
            .expect(path);
        assert_eq!(tracked.status(), 200, "{path}");
        let body: Value = tracked.json().await.expect("mixpanel json");
        assert_eq!(body["status"], 1, "{path}");
    }
    assert!(session.server.addr.port() > 0);
}

async fn assert_signed(session: &Session, token: &str, nonce: &str) {
    let jwks: Value = session
        .http
        .get(format!("{}/oauth2/jwks", session.base))
        .send()
        .await
        .expect("jwks")
        .json()
        .await
        .expect("jwks json");
    let key = &jwks["keys"][0];
    let payload = cortex_api::jwt::verify_with_jwk(
        token,
        key["n"].as_str().expect("n"),
        key["e"].as_str().expect("e"),
    )
    .expect("signature");
    assert_eq!(payload["iss"], "https://llm.imabee.com");
    assert_eq!(payload["aud"], cortex_api::CLIENT_ID);
    if !nonce.is_empty() {
        assert_eq!(payload["nonce"], nonce);
        assert!(payload.get("email").is_some());
    }
}

fn urlencoding(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let (_, query) = url.split_once('?')?;
    for pair in query.split('&') {
        let (k, _v) = pair.split_once('=')?;
        if k == key {
            return Some(
                url::form_urlencoded::parse(pair.as_bytes())
                    .next()
                    .map(|(_, value)| value.into_owned())
                    .unwrap_or_default(),
            );
        }
    }
    None
}
