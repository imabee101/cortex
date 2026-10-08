use argon2::Argon2;
use argon2::PasswordHasher;
use argon2::PasswordVerifier;
use argon2::password_hash::{PasswordHash, SaltString, rand_core::OsRng};
use axum::Form;
use axum::Json;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header::SET_COOKIE;
use axum::response::IntoResponse;
use axum::response::Redirect;
use axum::response::Response;
use chrono::{Duration, Utc};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::net::SocketAddr;

use crate::db::{AuthTx, DeviceGrant, RefreshConsume};
use crate::jwt::{self, b64url};
use crate::pages;
use crate::{AppState, CLIENT_ID, ISSUER, client_ip, oauth_error};

#[derive(Deserialize)]
pub struct AuthorizeQuery {
    response_type: String,
    client_id: String,
    redirect_uri: String,
    scope: String,
    code_challenge: String,
    code_challenge_method: String,
    state: String,
    nonce: String,
    #[serde(default)]
    referrer: Option<String>,
}

#[derive(Deserialize)]
pub struct LoginForm {
    email: String,
    password: String,
    next: String,
}

#[derive(Deserialize)]
pub struct RegisterForm {
    email: String,
    password: String,
    first_name: String,
    last_name: String,
    #[serde(default)]
    next: String,
}

#[derive(Deserialize)]
pub struct ConsentForm {
    state: String,
    decision: String,
}

#[derive(Deserialize)]
pub struct TokenForm {
    grant_type: String,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    redirect_uri: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    code_verifier: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    device_code: Option<String>,
}

#[derive(Deserialize)]
pub struct DeviceCodeForm {
    client_id: String,
    scope: String,
    #[serde(default)]
    referrer: Option<String>,
}

#[derive(Deserialize)]
pub struct DeviceDecideForm {
    user_code: String,
    decision: String,
}

#[derive(Deserialize)]
pub struct DeviceQuery {
    #[serde(default)]
    user_code: String,
}

pub async fn discovery() -> impl IntoResponse {
    (
        [(axum::http::header::CACHE_CONTROL, "public, max-age=3600")],
        Json(json!({
            "issuer": ISSUER,
            "authorization_endpoint": format!("{ISSUER}/authorize"),
            "token_endpoint": format!("{ISSUER}/oauth2/token"),
            "jwks_uri": format!("{ISSUER}/oauth2/jwks"),
            "response_types_supported": ["code"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["RS256"],
            "code_challenge_methods_supported": ["S256"],
            "grant_types_supported": [
                "authorization_code",
                "refresh_token",
                "urn:ietf:params:oauth:grant-type:device_code"
            ],
            "token_endpoint_auth_methods_supported": ["none"],
            "scopes_supported": [
                "openid",
                "profile",
                "email",
                "offline_access",
                "cortex-cli:access",
                "api:access",
                "conversations:read",
                "conversations:write",
                "workspaces:read",
                "workspaces:write"
            ]
        })),
    )
}

pub async fn jwks(State(state): State<AppState>) -> impl IntoResponse {
    Json(jwt::jwks_document(&state.keys().published))
}

pub async fn authorize(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AuthorizeQuery>,
) -> Response {
    if query.client_id != CLIENT_ID {
        return html(
            StatusCode::BAD_REQUEST,
            pages::message_page("Sign in", "Unknown client."),
        );
    }
    if !allowed_redirect(&query.redirect_uri) {
        return html(
            StatusCode::BAD_REQUEST,
            pages::message_page("Sign in", "Redirect is not allowed."),
        );
    }
    if query.response_type != "code" {
        return redirect_error(
            &query.redirect_uri,
            &query.state,
            "unsupported_response_type",
        );
    }
    if query.code_challenge_method != "S256"
        || query.code_challenge.is_empty()
        || query.nonce.is_empty()
    {
        return redirect_error(&query.redirect_uri, &query.state, "invalid_request");
    }
    let _ = query.referrer;
    let tx = AuthTx {
        state: query.state.clone(),
        client_id: query.client_id,
        redirect_uri: query.redirect_uri.clone(),
        code_challenge: query.code_challenge,
        nonce: query.nonce,
        scope: query.scope.clone(),
        user_id: None,
        code_hash: None,
        expires_at: Utc::now() + Duration::minutes(10),
        used_at: None,
    };
    if state.db.save_auth_tx(&tx).await.is_err() {
        return html(
            StatusCode::INTERNAL_SERVER_ERROR,
            pages::message_page("Sign in", "Try again."),
        );
    }
    let next = authorize_next(&query.state, &query.redirect_uri, &tx);
    if let Some(user_id) = session_user(&state, &headers).await {
        let Some(user) = state.db.user_by_id(user_id).await.ok().flatten() else {
            return html(
                StatusCode::UNAUTHORIZED,
                pages::login_page(&next, Some("Sign in again.")),
            );
        };
        return html(
            StatusCode::OK,
            pages::consent_page(&user.email, &query.state, &query.scope),
        );
    }
    html(StatusCode::OK, pages::login_page(&next, None))
}

fn authorize_next(state: &str, redirect_uri: &str, tx: &AuthTx) -> String {
    let mut pairs = vec![
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", redirect_uri),
        ("scope", tx.scope.as_str()),
        ("code_challenge", tx.code_challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("nonce", tx.nonce.as_str()),
    ];
    let _ = &mut pairs;
    format!(
        "/authorize?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}&nonce={}",
        enc(CLIENT_ID),
        enc(redirect_uri),
        enc(&tx.scope),
        enc(&tx.code_challenge),
        enc(state),
        enc(&tx.nonce),
    )
}

pub async fn login(
    State(state): State<AppState>,
    addr: axum::extract::ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    if !safe_next(&form.next) {
        return html(
            StatusCode::BAD_REQUEST,
            pages::message_page("Sign in", "That return path is not allowed."),
        );
    }
    let email = normalize_email(&form.email);
    let ip = client_ip(addr.0, &headers);
    if let Ok(Some(retry)) = state
        .db
        .lock_retry(&ip, &email, state.config.login_limit)
        .await
    {
        return retry_page(retry);
    }
    if state
        .db
        .hit_rate(
            &format!("login:{ip}"),
            state.config.login_limit,
            state.config.login_window_secs,
        )
        .await
        .ok()
        .filter(|rate| !rate.allowed)
        .is_some()
    {
        return retry_page(state.config.login_window_secs);
    }
    let user = state.db.user_by_email(&email).await.ok().flatten();
    let password = form.password;
    let ok = match &user {
        Some(user) => verify_password(&password, &user.password_hash).await,
        None => false,
    };
    let _ = state
        .db
        .record_login(
            &ip,
            &email,
            ok,
            state.config.lock_after,
            state.config.lock_secs,
        )
        .await;
    if !ok {
        return html(
            StatusCode::UNAUTHORIZED,
            pages::login_page(&form.next, Some("Email or password is wrong.")),
        );
    }
    let user = user.expect("password matched a user");
    match open_session(&state, user.id).await {
        Ok(cookie) => see_other(cookie, &form.next, "Sign in"),
        Err(()) => html(
            StatusCode::INTERNAL_SERVER_ERROR,
            pages::message_page("Sign in", "Try again."),
        ),
    }
}

#[derive(Deserialize)]
pub struct NextQuery {
    #[serde(default)]
    next: String,
}

pub async fn register_form(Query(query): Query<NextQuery>) -> impl IntoResponse {
    let next = if query.next.is_empty() || !safe_next(&query.next) {
        "/authorize".to_owned()
    } else {
        query.next
    };
    HtmlPage(pages::register_page(&next, None))
}

pub async fn register(
    State(state): State<AppState>,
    addr: axum::extract::ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    Form(form): Form<RegisterForm>,
) -> Response {
    let next = if safe_next(&form.next) {
        form.next.clone()
    } else {
        "/authorize".to_owned()
    };
    let ip = client_ip(addr.0, &headers);
    if let Ok(rate) = state
        .db
        .hit_rate(
            &format!("register:{ip}"),
            state.config.register_limit,
            state.config.register_window_secs,
        )
        .await
        && !rate.allowed
    {
        return retry_page(rate.retry_after);
    }
    let email = normalize_email(&form.email);
    if !email.contains('@')
        || email.contains(' ')
        || form.first_name.trim().is_empty()
        || form.last_name.trim().is_empty()
    {
        return html(
            StatusCode::BAD_REQUEST,
            pages::register_page(&next, Some("Enter an email and both names.")),
        );
    }
    if form.password.chars().count() < 12 || form.password.chars().count() > 128 {
        return html(
            StatusCode::BAD_REQUEST,
            pages::register_page(&next, Some("Use a password of 12 to 128 characters.")),
        );
    }
    let password = form.password.clone();
    let hash = match tokio::task::spawn_blocking(move || hash_password(&password)).await {
        Ok(Ok(hash)) => hash,
        _ => {
            return html(
                StatusCode::INTERNAL_SERVER_ERROR,
                pages::message_page("Create account", "Try again."),
            );
        }
    };
    let user_id = match state
        .db
        .insert_user(&email, &hash, form.first_name.trim(), form.last_name.trim())
        .await
    {
        Ok(id) => id,
        Err(_) => {
            return html(
                StatusCode::CONFLICT,
                pages::register_page(&next, Some("An account with that email already exists.")),
            );
        }
    };
    match open_session(&state, user_id).await {
        Ok(cookie) => see_other(cookie, &next, "Create account"),
        Err(()) => html(
            StatusCode::INTERNAL_SERVER_ERROR,
            pages::message_page("Create account", "Try again."),
        ),
    }
}

pub async fn consent(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ConsentForm>,
) -> Response {
    let Some(user_id) = session_user(&state, &headers).await else {
        return html(
            StatusCode::UNAUTHORIZED,
            pages::message_page("Allow access", "Sign in first."),
        );
    };
    let Some(tx) = state.db.auth_tx(&form.state).await.ok().flatten() else {
        return html(
            StatusCode::BAD_REQUEST,
            pages::message_page("Allow access", "This sign-in expired."),
        );
    };
    if !allowed_redirect(&tx.redirect_uri) {
        return html(
            StatusCode::BAD_REQUEST,
            pages::message_page("Allow access", "Redirect is not allowed."),
        );
    }
    if form.decision == "deny" {
        return redirect_error(&tx.redirect_uri, &tx.state, "access_denied");
    }
    if form.decision != "allow" {
        return html(
            StatusCode::BAD_REQUEST,
            pages::message_page("Allow access", "Choose allow or deny."),
        );
    }
    let code = jwt::random_token();
    let code_hash = jwt::sha256_hex(code.as_bytes());
    let expires = Utc::now() + Duration::seconds(jwt::CODE_TTL_SECS);
    match state
        .db
        .issue_code(&tx.state, user_id, &code_hash, expires)
        .await
    {
        Ok(true) => Redirect::to(&format!(
            "{}?code={}&state={}",
            tx.redirect_uri,
            enc(&code),
            enc(&tx.state)
        ))
        .into_response(),
        _ => html(
            StatusCode::CONFLICT,
            pages::message_page("Allow access", "This sign-in was already used."),
        ),
    }
}

pub async fn token(
    State(state): State<AppState>,
    addr: axum::extract::ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    Form(form): Form<TokenForm>,
) -> Response {
    let ip = client_ip(addr.0, &headers);
    if let Ok(rate) = state.db.hit_rate(&format!("token:{ip}"), 60, 60).await
        && !rate.allowed
    {
        return oauth_error(StatusCode::TOO_MANY_REQUESTS, "slow_down", rate.retry_after);
    }
    if form.client_id.as_deref().is_some_and(|id| id != CLIENT_ID) {
        return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client", 0);
    }
    match form.grant_type.as_str() {
        "authorization_code" => code_grant(&state, &form).await,
        "refresh_token" => refresh_grant(&state, &form).await,
        "urn:ietf:params:oauth:grant-type:device_code" => device_grant(&state, &form).await,
        _ => oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type", 0),
    }
}

async fn code_grant(state: &AppState, form: &TokenForm) -> Response {
    let (Some(code), Some(verifier), Some(redirect)) = (
        form.code.as_deref(),
        form.code_verifier.as_deref(),
        form.redirect_uri.as_deref(),
    ) else {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_request", 0);
    };
    let hash = jwt::sha256_hex(code.as_bytes());
    let Some(tx) = state.db.consume_code(&hash).await.ok().flatten() else {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", 0);
    };
    if tx.redirect_uri != redirect || !pkce_ok(verifier, &tx.code_challenge) {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", 0);
    }
    let Some(user_id) = tx.user_id else {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", 0);
    };
    issue_tokens(state, user_id, &tx.scope, Some(&tx.nonce), None).await
}

async fn refresh_grant(state: &AppState, form: &TokenForm) -> Response {
    let Some(token) = form.refresh_token.as_deref() else {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_request", 0);
    };
    let hash = jwt::sha256_hex(token.as_bytes());
    match state.db.consume_refresh(&hash).await {
        Ok(RefreshConsume::Ok(row)) => {
            issue_tokens(state, row.user_id, &row.scope, None, Some(row.family_id)).await
        }
        Ok(RefreshConsume::Reuse(family)) => {
            let _ = state.db.revoke_family(family).await;
            oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", 0)
        }
        _ => oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", 0),
    }
}

async fn device_grant(state: &AppState, form: &TokenForm) -> Response {
    let Some(code) = form.device_code.as_deref() else {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_request", 0);
    };
    let hash = jwt::sha256_hex(code.as_bytes());
    let Some(grant) = state.db.device_by_hash(&hash).await.ok().flatten() else {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", 0);
    };
    if grant.expires_at <= Utc::now() {
        return oauth_error(StatusCode::BAD_REQUEST, "expired_token", 0);
    }
    if grant.consumed_at.is_some() {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", 0);
    }
    if let Some(after) = grant.poll_after
        && Utc::now() < after
    {
        return oauth_error(StatusCode::BAD_REQUEST, "slow_down", grant.interval_secs);
    }
    let next_poll = Utc::now() + Duration::seconds(i64::from(grant.interval_secs.max(1)));
    let _ = state.db.mark_device_poll(&hash, next_poll).await;
    match grant.status.as_str() {
        "pending" => oauth_error(StatusCode::BAD_REQUEST, "authorization_pending", 0),
        "denied" => oauth_error(StatusCode::BAD_REQUEST, "access_denied", 0),
        "approved" => {
            let Some(consumed) = state.db.consume_device(&hash).await.ok().flatten() else {
                return oauth_error(StatusCode::BAD_REQUEST, "authorization_pending", 0);
            };
            let Some(user_id) = consumed.user_id else {
                return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", 0);
            };
            issue_tokens(state, user_id, &consumed.scope, None, None).await
        }
        _ => oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", 0),
    }
}

pub async fn device_code(
    State(state): State<AppState>,
    addr: axum::extract::ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    Form(form): Form<DeviceCodeForm>,
) -> Response {
    if form.client_id != CLIENT_ID {
        return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client", 0);
    }
    let ip = client_ip(addr.0, &headers);
    if let Ok(rate) = state.db.hit_rate(&format!("device:{ip}"), 10, 600).await
        && !rate.allowed
    {
        return oauth_error(StatusCode::TOO_MANY_REQUESTS, "slow_down", rate.retry_after);
    }
    let _ = form.referrer;
    let device = jwt::random_token();
    let user_code = jwt::user_code();
    let grant = DeviceGrant {
        device_code_hash: jwt::sha256_hex(device.as_bytes()),
        user_code: user_code.clone(),
        client_id: form.client_id,
        scope: form.scope,
        status: "pending".to_owned(),
        user_id: None,
        expires_at: Utc::now() + Duration::seconds(jwt::DEVICE_TTL_SECS),
        interval_secs: jwt::DEVICE_INTERVAL_SECS,
        poll_after: None,
        consumed_at: None,
    };
    if state.db.insert_device(&grant).await.is_err() {
        return oauth_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", 0);
    }
    let verification_uri = format!("{ISSUER}/device");
    Json(json!({
        "device_code": device,
        "user_code": user_code,
        "verification_uri": verification_uri,
        "verification_uri_complete": format!("{verification_uri}?user_code={}", enc(&grant.user_code)),
        "expires_in": jwt::DEVICE_TTL_SECS,
        "interval": jwt::DEVICE_INTERVAL_SECS,
    }))
    .into_response()
}

pub async fn device_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<DeviceQuery>,
) -> Response {
    let signed_in = session_user(&state, &headers).await.is_some();
    let next = format!("/device?user_code={}", enc(&query.user_code));
    html(
        StatusCode::OK,
        pages::device_page(&query.user_code, signed_in, &next),
    )
}

pub async fn device_decide(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<DeviceDecideForm>,
) -> Response {
    let Some(user_id) = session_user(&state, &headers).await else {
        return html(
            StatusCode::UNAUTHORIZED,
            pages::message_page("Confirm device", "Sign in first."),
        );
    };
    let user_code = normalize_user_code(&form.user_code);
    let status = match form.decision.as_str() {
        "approve" => "approved",
        "deny" => "denied",
        _ => {
            return html(
                StatusCode::BAD_REQUEST,
                pages::message_page("Confirm device", "Choose approve or deny."),
            );
        }
    };
    match state.db.decide_device(&user_code, user_id, status).await {
        Ok(true) if status == "approved" => html(
            StatusCode::OK,
            pages::message_page("Confirm device", "Device approved. Return to the CLI."),
        ),
        Ok(true) => html(
            StatusCode::OK,
            pages::message_page("Confirm device", "Device denied."),
        ),
        _ => html(
            StatusCode::BAD_REQUEST,
            pages::message_page("Confirm device", "That code is expired or already used."),
        ),
    }
}

async fn issue_tokens(
    state: &AppState,
    user_id: uuid::Uuid,
    scope: &str,
    nonce: Option<&str>,
    family: Option<uuid::Uuid>,
) -> Response {
    let Some(user) = state.db.user_by_id(user_id).await.ok().flatten() else {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", 0);
    };
    let now = Utc::now().timestamp();
    let keys = state.keys();
    let access = match jwt::sign(
        &keys.active.private,
        &keys.active.kid,
        &json!({
            "iss": ISSUER,
            "sub": user.id.to_string(),
            "aud": CLIENT_ID,
            "iat": now,
            "exp": now + jwt::ACCESS_TTL_SECS,
            "scope": scope,
            "principal_type": "User",
            "principal_id": user.id.to_string(),
        }),
    ) {
        Ok(token) => token,
        Err(_) => return oauth_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", 0),
    };
    let mut id_claims = json!({
        "iss": ISSUER,
        "sub": user.id.to_string(),
        "aud": CLIENT_ID,
        "iat": now,
        "exp": now + jwt::ACCESS_TTL_SECS,
        "email": user.email,
        "given_name": user.first_name,
        "family_name": user.last_name,
    });
    if let Some(nonce) = nonce {
        id_claims["nonce"] = json!(nonce);
    }
    let id_token = match jwt::sign(&keys.active.private, &keys.active.kid, &id_claims) {
        Ok(token) => token,
        Err(_) => return oauth_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", 0),
    };
    let refresh = jwt::random_token();
    let family = family.unwrap_or_else(uuid::Uuid::new_v4);
    if state
        .db
        .insert_refresh(
            &jwt::sha256_hex(refresh.as_bytes()),
            family,
            user.id,
            scope,
            Utc::now() + Duration::seconds(jwt::REFRESH_TTL_SECS),
        )
        .await
        .is_err()
    {
        return oauth_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", 0);
    }
    (
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "access_token": access,
            "token_type": "Bearer",
            "expires_in": jwt::ACCESS_TTL_SECS,
            "refresh_token": refresh,
            "scope": scope,
            "id_token": id_token,
        })),
    )
        .into_response()
}

pub fn hash_password(password: &str) -> Result<String, ()> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|_| ())
}

async fn verify_password(password: &str, encoded: &str) -> bool {
    let password = password.to_owned();
    let encoded = encoded.to_owned();
    tokio::task::spawn_blocking(move || {
        let Ok(parsed) = PasswordHash::new(&encoded) else {
            return false;
        };
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
    .await
    .unwrap_or(false)
}

async fn open_session(
    state: &AppState,
    user_id: uuid::Uuid,
) -> Result<axum::http::HeaderValue, ()> {
    let id = jwt::random_token();
    let expires = Utc::now() + Duration::hours(12);
    state
        .db
        .insert_session(&id, user_id, expires)
        .await
        .map_err(|_| ())?;
    let value = format!("session={id}; HttpOnly; SameSite=Lax; Path=/; Max-Age=43200");
    axum::http::HeaderValue::from_str(&value).map_err(|_| ())
}

async fn session_user(state: &AppState, headers: &HeaderMap) -> Option<uuid::Uuid> {
    let cookie = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    let id = cookie.split(';').find_map(|part| {
        let part = part.trim();
        part.strip_prefix("session=")
    })?;
    state.db.session_user(id).await.ok().flatten()
}

pub fn allowed_redirect(uri: &str) -> bool {
    let Ok(parsed) = url::Url::parse(uri) else {
        return false;
    };
    if parsed.scheme() != "http" || parsed.path() != "/callback" || parsed.query().is_some() {
        return false;
    }
    matches!(parsed.host_str(), Some("127.0.0.1") | Some("localhost"))
}

fn see_other(cookie: axum::http::HeaderValue, next: &str, title: &str) -> Response {
    let Ok(location) = axum::http::HeaderValue::from_str(next) else {
        return html(
            StatusCode::INTERNAL_SERVER_ERROR,
            pages::message_page(title, "Try again."),
        );
    };
    (
        StatusCode::SEE_OTHER,
        [
            (SET_COOKIE, cookie),
            (axum::http::header::LOCATION, location),
        ],
    )
        .into_response()
}

fn safe_next(next: &str) -> bool {
    (next.starts_with("/authorize?") || next.starts_with("/device?") || next == "/authorize")
        && !next.contains("://")
        && !next.contains('\\')
}

fn normalize_email(email: &str) -> String {
    email.trim().to_ascii_lowercase()
}

fn normalize_user_code(code: &str) -> String {
    let compact: String = code.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let upper = compact.to_ascii_uppercase();
    if upper.len() == 8 {
        format!("{}-{}", &upper[..4], &upper[4..])
    } else {
        code.trim().to_ascii_uppercase()
    }
}

fn pkce_ok(verifier: &str, challenge: &str) -> bool {
    if !(43..=128).contains(&verifier.len()) {
        return false;
    }
    b64url(&Sha256::digest(verifier.as_bytes())) == challenge
}

fn enc(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn redirect_error(uri: &str, state: &str, error: &str) -> Response {
    Redirect::to(&format!("{uri}?error={}&state={}", enc(error), enc(state))).into_response()
}

fn html(status: StatusCode, body: String) -> Response {
    (status, HtmlPage(body)).into_response()
}

fn retry_page(secs: i32) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(axum::http::header::RETRY_AFTER, secs.max(1).to_string())],
        HtmlPage(pages::message_page("Try again later", "Too many attempts.")),
    )
        .into_response()
}

struct HtmlPage(String);

impl IntoResponse for HtmlPage {
    fn into_response(self) -> Response {
        (
            [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
            self.0,
        )
            .into_response()
    }
}
