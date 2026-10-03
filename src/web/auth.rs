//! A single shared password, exchanged at the login page for a session cookie.
//!
//! The cookie is HttpOnly and SameSite=Strict, so other sites can neither read it nor make the
//! browser send it with their requests. Only a hash of each session token is stored.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::AppError;
use crate::{Ctx, db};

const COOKIE: &str = "tl_session";
const SESSION_DAYS: i64 = 30;

/// Reachable without a session: the login page and what it needs.
const PUBLIC: &[&str] = &["/login.html", "/style.css", "/api/login", "/api/auth"];

pub fn hash(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Comparing hashes in constant time leaks nothing about how much of a guess was right.
fn same(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn session_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| pair.trim().strip_prefix(COOKIE)?.strip_prefix('='))
        .filter(|token| !token.is_empty())
}

async fn has_session(ctx: &Ctx, headers: &HeaderMap) -> bool {
    match session_token(headers) {
        Some(token) => db::web_session_valid(&ctx.pool, &hex(&hash(token)))
            .await
            .unwrap_or(false),
        None => false,
    }
}

/// Middleware: everything but `PUBLIC` needs a session once a password is set. Pages redirect to
/// the login page; API calls and media get a plain 401.
pub(super) async fn require(State(ctx): State<Arc<Ctx>>, request: Request, next: Next) -> Response {
    let path = request.uri().path();
    if ctx.password_hash.is_none() || PUBLIC.contains(&path) || has_session(&ctx, request.headers()).await {
        return next.run(request).await;
    }
    if path.starts_with("/api/") || path.starts_with("/streams/") {
        return AppError::Unauthorized.into_response();
    }
    Redirect::to("/login.html").into_response()
}

#[derive(Deserialize)]
pub(super) struct Login {
    password: String,
}

pub(super) async fn login(State(ctx): State<Arc<Ctx>>, Json(login): Json<Login>) -> Result<Response, AppError> {
    let Some(expected) = &ctx.password_hash else {
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    if !same(&hash(&login.password), expected) {
        // Makes guessing slow without any bookkeeping.
        tokio::time::sleep(Duration::from_secs(1)).await;
        return Err(AppError::Unauthorized);
    }
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    let token = hex(&raw);
    let expires = db::now_ms() + SESSION_DAYS * 86_400_000;
    db::create_web_session(&ctx.pool, &hex(&hash(&token)), expires).await?;
    let cookie = format!(
        "{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
        SESSION_DAYS * 86_400
    );
    Ok((StatusCode::NO_CONTENT, [(header::SET_COOKIE, cookie)]).into_response())
}

pub(super) async fn logout(State(ctx): State<Arc<Ctx>>, headers: HeaderMap) -> Result<Response, AppError> {
    if let Some(token) = session_token(&headers) {
        db::delete_web_session(&ctx.pool, &hex(&hash(token))).await?;
    }
    let expired = HeaderValue::from_static("tl_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0");
    Ok((StatusCode::NO_CONTENT, [(header::SET_COOKIE, expired)]).into_response())
}

/// Whether a password is set and whether this browser is logged in, for the UI.
pub(super) async fn status(State(ctx): State<Arc<Ctx>>, headers: HeaderMap) -> Response {
    let required = ctx.password_hash.is_some();
    let logged_in = !required || has_session(&ctx, &headers).await;
    Json(serde_json::json!({ "required": required, "logged_in": logged_in })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_session_cookie_among_others() {
        let mut headers = HeaderMap::new();
        headers.append(header::COOKIE, HeaderValue::from_static("a=1; tl_session=abc; b=2"));
        assert_eq!(session_token(&headers), Some("abc"));
        let mut headers = HeaderMap::new();
        headers.append(header::COOKIE, HeaderValue::from_static("tl_sessionx=abc; tl_session="));
        assert_eq!(session_token(&headers), None);
    }

    #[test]
    fn compares_hashes() {
        assert!(same(&hash("secret"), &hash("secret")));
        assert!(!same(&hash("secret"), &hash("Secret")));
    }
}
