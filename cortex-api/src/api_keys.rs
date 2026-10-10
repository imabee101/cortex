//! The browser console where a signed-in user creates, lists, and revokes API keys.
//!
//! A key is `cortex-` plus 256 random bits. Only its SHA-256 and last four characters are
//! stored, so the plaintext is shown exactly once, on the page that created it. Every form
//! carries a token derived from the browser session, on top of the `SameSite=Lax` cookie.

use axum::Form;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header::{CACHE_CONTROL, REFERRER_POLICY};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use uuid::Uuid;

use crate::oidc::{browser_session, html};
use crate::{API_KEY_PREFIX, AppState, jwt, pages};

pub const CONSOLE_PATH: &str = "/account/api-keys";
const NAME_MAX_CHARS: usize = 64;
const TITLE: &str = "API keys";

/// Console pages show account data and, once, a secret: never cached, never framed, never
/// sent as a referrer.
fn private(response: Response) -> Response {
    (
        [
            (CACHE_CONTROL, "no-store"),
            (REFERRER_POLICY, "no-referrer"),
            (
                axum::http::header::CONTENT_SECURITY_POLICY,
                "frame-ancestors 'none'",
            ),
        ],
        response,
    )
        .into_response()
}

fn form_token(session_id: &str) -> String {
    jwt::sha256_hex(format!("api-keys:{session_id}").as_bytes())
}

async fn console(
    state: &AppState,
    user: Uuid,
    session_id: &str,
    notice: pages::KeyNotice<'_>,
) -> Response {
    match state.db.api_keys_for(user).await {
        Ok(keys) => private(html(
            StatusCode::OK,
            pages::api_keys_page(&keys, &form_token(session_id), notice),
        )),
        Err(_) => private(html(
            StatusCode::SERVICE_UNAVAILABLE,
            pages::message_page(TITLE, "Try again."),
        )),
    }
}

fn signed_out() -> Response {
    private(html(StatusCode::OK, pages::login_page(CONSOLE_PATH, None)))
}

fn forbidden() -> Response {
    private(html(
        StatusCode::FORBIDDEN,
        pages::message_page(TITLE, "This form expired. Reload the page and try again."),
    ))
}

pub async fn page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some((user, session_id)) = browser_session(&state, &headers).await else {
        return signed_out();
    };
    console(&state, user, &session_id, pages::KeyNotice::None).await
}

#[derive(Deserialize)]
pub struct CreateForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    name: String,
}

pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CreateForm>,
) -> Response {
    let Some((user, session_id)) = browser_session(&state, &headers).await else {
        return signed_out();
    };
    if form.csrf != form_token(&session_id) {
        return forbidden();
    }
    let name = form.name.trim();
    if name.is_empty()
        || name.chars().count() > NAME_MAX_CHARS
        || name.chars().any(char::is_control)
    {
        return console(
            &state,
            user,
            &session_id,
            pages::KeyNotice::Error("Give the key a name of 1 to 64 characters."),
        )
        .await;
    }
    let key = format!("{API_KEY_PREFIX}{}", jwt::random_token());
    let suffix: String = key
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let created = state
        .db
        .insert_api_key(
            Uuid::new_v4(),
            user,
            name,
            &jwt::sha256_hex(key.as_bytes()),
            &suffix,
            state.config.api_key_limit,
        )
        .await;
    match created {
        Ok(true) => {
            tracing::info!(%user, "api key created");
            console(&state, user, &session_id, pages::KeyNotice::Created(&key)).await
        }
        Ok(false) => {
            console(
                &state,
                user,
                &session_id,
                pages::KeyNotice::Error(
                    "You have reached the key limit. Revoke a key before creating another.",
                ),
            )
            .await
        }
        Err(_) => private(html(
            StatusCode::SERVICE_UNAVAILABLE,
            pages::message_page(TITLE, "Try again."),
        )),
    }
}

#[derive(Deserialize)]
pub struct RevokeForm {
    #[serde(default)]
    csrf: String,
    id: Uuid,
}

pub async fn revoke(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<RevokeForm>,
) -> Response {
    let Some((user, session_id)) = browser_session(&state, &headers).await else {
        return signed_out();
    };
    if form.csrf != form_token(&session_id) {
        return forbidden();
    }
    match state.db.revoke_api_key(user, form.id).await {
        Ok(true) => {
            tracing::info!(%user, key = %form.id, "api key revoked");
            console(&state, user, &session_id, pages::KeyNotice::Revoked).await
        }
        Ok(false) => private(html(
            StatusCode::NOT_FOUND,
            pages::message_page(TITLE, "No such key."),
        )),
        Err(_) => private(html(
            StatusCode::SERVICE_UNAVAILABLE,
            pages::message_page(TITLE, "Try again."),
        )),
    }
}
