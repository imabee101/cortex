// Test clients talk to the loopback server only.
#![allow(clippy::disallowed_methods)]
use std::time::Duration;

use reqwest::StatusCode;
use serde_json::Value;
use sha2::{Digest, Sha256};

// A database of its own: test binaries run in parallel, and the server creates it on start.
const DB_NAME: &str = "cortex_api_keys_test";

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .cookie_store(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .expect("client")
}

fn psql(sql: &str) -> String {
    let out = std::process::Command::new("psql")
        .args([
            "-h",
            "127.0.0.1",
            "-d",
            DB_NAME,
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

/// The value of the first `name="csrf"` hidden field on a console page.
fn csrf(page: &str) -> String {
    let marker = r#"name="csrf" value=""#;
    let start = page.find(marker).expect("csrf field") + marker.len();
    page[start..start + 64].to_owned()
}

/// The plaintext key shown on the page that created it.
fn shown_key(page: &str) -> String {
    let start =
        page.find(r#"<code class="secret">"#).expect("new key") + r#"<code class="secret">"#.len();
    let end = start + page[start..].find("</code>").expect("key end");
    page[start..end].to_owned()
}

async fn register(http: &reqwest::Client, base: &str, email: &str) {
    let response = http
        .post(format!("{base}/register"))
        .form(&[
            ("email", email),
            ("password", "correct-horse-battery"),
            ("first_name", "Key"),
            ("last_name", "Holder"),
            ("next", "/account/api-keys"),
        ])
        .send()
        .await
        .expect("register");
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers()["location"].to_str().expect("location"),
        "/account/api-keys"
    );
}

async fn console(http: &reqwest::Client, base: &str) -> String {
    let response = http
        .get(format!("{base}/account/api-keys"))
        .send()
        .await
        .expect("console");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(
        response.headers()["content-security-policy"],
        "frame-ancestors 'none'"
    );
    response.text().await.expect("console body")
}

async fn create(
    http: &reqwest::Client,
    base: &str,
    csrf: &str,
    name: &str,
) -> (StatusCode, String) {
    let response = http
        .post(format!("{base}/account/api-keys"))
        .form(&[("csrf", csrf), ("name", name)])
        .send()
        .await
        .expect("create");
    (
        response.status(),
        response.text().await.expect("create body"),
    )
}

async fn revoke(http: &reqwest::Client, base: &str, csrf: &str, id: &str) -> (StatusCode, String) {
    let response = http
        .post(format!("{base}/account/api-keys/revoke"))
        .form(&[("csrf", csrf), ("id", id)])
        .send()
        .await
        .expect("revoke");
    (
        response.status(),
        response.text().await.expect("revoke body"),
    )
}

async fn probe(base: &str, bearer: Option<&str>, session_marker: bool) -> reqwest::Response {
    let mut request = http_client().get(format!("{base}/v1/api-key"));
    if let Some(bearer) = bearer {
        request = request.bearer_auth(bearer);
    }
    if session_marker {
        request = request.header("x-cortex-token-auth", "cortex-cli");
    }
    request.send().await.expect("probe")
}

#[tokio::test]
async fn api_keys_are_created_used_and_revoked_through_the_console() {
    let mut config = cortex_api::Config::new(
        "127.0.0.1:0".parse().expect("addr"),
        format!("postgres://127.0.0.1/{DB_NAME}"),
    );
    config.api_key_limit = 2;
    let server = cortex_api::serve(config).await.expect("serve");
    // After serve, so the migrations have created every table.
    psql("TRUNCATE users, browser_sessions, login_attempts, rate_buckets CASCADE");
    let base = format!("http://{}", server.addr);

    // Signed out, the console asks for a sign-in that returns to it.
    let signed_out = http_client()
        .get(format!("{base}/account/api-keys"))
        .send()
        .await
        .expect("signed out")
        .text()
        .await
        .expect("signed out body");
    assert!(signed_out.contains(r#"action="/login""#));
    assert!(signed_out.contains(r#"name="next" value="/account/api-keys""#));

    let alice = http_client();
    register(&alice, &base, "alice@example.com").await;
    let page = console(&alice, &base).await;
    assert!(page.contains("No keys yet."));
    let alice_csrf = csrf(&page);

    // A form without the session's token, or without a usable name, creates nothing.
    let (status, _) = create(&alice, &base, &"0".repeat(64), "forged").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = create(&alice, &base, &alice_csrf, "   ").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Give the key a name"));
    assert_eq!(psql("SELECT count(*) FROM api_keys"), "0");

    let (status, body) = create(&alice, &base, &alice_csrf, "ci").await;
    assert_eq!(status, StatusCode::OK);
    let key = shown_key(&body);
    assert!(key.starts_with("cortex-"));
    assert_eq!(key.len(), "cortex-".len() + 43);
    assert!(body.contains(&format!(r#"export CORTEX_API_KEY="{key}""#)));

    // Only the hash and the last four characters are stored, and the plaintext is never shown again.
    let hash = format!("{:x}", Sha256::digest(key.as_bytes()));
    assert_eq!(psql("SELECT key_hash FROM api_keys"), hash);
    let stored = psql("SELECT id::text || name || key_hash || key_suffix FROM api_keys");
    assert!(!stored.contains(&key[7..]));
    let reloaded = console(&alice, &base).await;
    assert!(!reloaded.contains(&key));
    assert!(reloaded.contains(&format!("cortex-…{}", &key[key.len() - 4..])));
    assert!(reloaded.contains("Never"));

    // The probe the client sends before advertising the key.
    let response = probe(&base, Some(&key), false).await;
    assert_eq!(response.status(), StatusCode::OK);
    let info: Value = response.json().await.expect("probe json");
    let key_id = psql("SELECT id FROM api_keys");
    let alice_id = psql("SELECT id FROM users WHERE email = 'alice@example.com'");
    assert_eq!(info["api_key_id"], key_id.as_str());
    assert_eq!(info["user_id"], alice_id.as_str());
    assert_eq!(info["name"], "ci");
    assert_eq!(info["api_key_blocked"], false);
    assert_eq!(info["api_key_disabled"], false);
    assert_eq!(info["team_blocked"], false);
    assert_eq!(
        info["redacted_api_key"],
        format!("cortex-…{}", &key[key.len() - 4..])
    );
    assert_eq!(psql("SELECT last_used_at IS NOT NULL FROM api_keys"), "t");

    // The key authenticates any route a session token does.
    let models = http_client()
        .get(format!("{base}/v1/models"))
        .bearer_auth(&key)
        .send()
        .await
        .expect("models");
    assert_eq!(models.status(), StatusCode::OK);

    // A request marked as a session token is never checked as a key; unknown or missing keys fail.
    assert_eq!(
        probe(&base, Some(&key), true).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let unknown = format!("cortex-{}", "A".repeat(43));
    assert_eq!(
        probe(&base, Some(&unknown), false).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        probe(&base, None, false).await.status(),
        StatusCode::UNAUTHORIZED
    );

    // The per-account limit holds.
    let (_, body) = create(&alice, &base, &alice_csrf, "second").await;
    let second = shown_key(&body);
    let (_, body) = create(&alice, &base, &alice_csrf, "third").await;
    assert!(body.contains("reached the key limit"));
    assert_eq!(
        psql("SELECT count(*) FROM api_keys WHERE revoked_at IS NULL"),
        "2"
    );

    // Another account can neither revoke Alice's key nor reuse her form token.
    let bob = http_client();
    register(&bob, &base, "bob@example.com").await;
    let bob_csrf = csrf(&console(&bob, &base).await);
    assert_ne!(bob_csrf, alice_csrf);
    let (status, _) = revoke(&bob, &base, &bob_csrf, &key_id).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = revoke(&bob, &base, &alice_csrf, &key_id).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!console(&bob, &base).await.contains("cortex-…"));
    assert_eq!(
        probe(&base, Some(&key), false).await.status(),
        StatusCode::OK
    );

    // Revocation takes effect on the next request and leaves the other key working.
    let (status, body) = revoke(&alice, &base, &alice_csrf, &key_id).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Key revoked"));
    assert!(!body.contains(">ci<"));
    assert_eq!(
        probe(&base, Some(&key), false).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        probe(&base, Some(&second), false).await.status(),
        StatusCode::OK
    );

    server.shutdown().await.expect("shutdown");
}
