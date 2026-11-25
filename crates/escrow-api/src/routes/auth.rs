//! GitHub sign-in, the session, wallet linking and the signed-in user's own data.

use crate::{
    error::{ApiError, ApiResult},
    extract::{ApiJson, ClientIp},
    metrics::Metrics,
    session::{self, limit_key, require_user},
    state::AppState,
    wallet,
};
use axum::{
    Json,
    extract::{Extension, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use escrow_core::{
    oauth::{OAUTH_COOKIE, StateError, clear_cookie, cookie_value, safe_next, session_cookie, start_login, state_cookie, verify_state},
    security::csrf_token,
};
use escrow_db::{bounties, payouts, users};
use escrow_github::OAuthError;
use escrow_payouts::on_wallet_linked;
use serde::Deserialize;
use serde_json::json;
use std::time::Instant;

/// Per-user (or per-IP) budget for state-changing calls.
pub fn mutation_limit(s: &AppState, h: &HeaderMap, ip: &ClientIp, max: u32) -> ApiResult<()> {
    if s.limiter.check(&format!("m:{}", limit_key(h, &ip.0)), max, Instant::now()) {
        Ok(())
    } else {
        Metrics::inc(&s.metrics.http_rate_limited);
        Err(ApiError::rate_limited())
    }
}

fn login_not_configured() -> ApiError {
    ApiError::new(501, "login_not_configured", "GitHub sign-in is not configured on this server")
}

fn callback_url(s: &AppState) -> String {
    format!("{}/auth/callback", s.cfg.api_public_url)
}

#[derive(Deserialize)]
pub struct LoginQuery {
    next: Option<String>,
}

pub async fn login(State(s): State<AppState>, Query(q): Query<LoginQuery>) -> ApiResult<Response> {
    let login = s.login.as_ref().ok_or_else(login_not_configured)?;
    let started = start_login(login.client_id(), &callback_url(&s), &s.cfg.session_secret, q.next.as_deref(), (s.now)().timestamp_millis());
    let mut r = Redirect::to(&started.authorize_url).into_response();
    r.headers_mut().append(header::SET_COOKIE, state_cookie(&started.cookie_value, s.cfg.secure_cookies).parse().expect("cookie is ascii"));
    Ok(r)
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
}

pub async fn callback(State(s): State<AppState>, headers: HeaderMap, Query(q): Query<CallbackQuery>) -> ApiResult<Response> {
    let login = s.login.as_ref().ok_or_else(login_not_configured)?;
    let clear = || [(header::SET_COOKIE, clear_cookie(OAUTH_COOKIE))];
    let fail = |status, code: &str, msg: &str| {
        let mut r = ApiError::new(status, code, msg).into_response();
        r.headers_mut().append(header::SET_COOKIE, clear_cookie(OAUTH_COOKIE).parse().expect("ascii"));
        Ok(r)
    };

    let cookie = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()).and_then(|c| cookie_value(c, OAUTH_COOKIE));
    let (Some(cookie), Some(code), Some(state)) = (cookie, q.code.as_deref(), q.state.as_deref()) else {
        return fail(400, "bad_request", "missing login state");
    };
    let st = match verify_state(&s.cfg.session_secret, cookie, state, (s.now)().timestamp_millis()) {
        Ok(st) => st,
        Err(StateError::Expired) => return fail(400, "state_expired", "login took too long, please try again"),
        Err(StateError::Mismatch) => return fail(400, "bad_state", "login state does not match"),
        Err(StateError::Invalid) => return fail(400, "bad_state", "login state is invalid"),
    };
    let token = match login.exchange_code(code, &callback_url(&s), &st.verifier).await {
        Ok(t) => t,
        Err(OAuthError::Rejected(why)) => {
            return fail(
                400,
                "github_rejected",
                &format!("GitHub rejected the login{}", if why.is_empty() { String::new() } else { format!(" ({why})") }),
            );
        }
        Err(OAuthError::Unavailable) => return fail(502, "github_unavailable", "could not reach GitHub"),
    };
    let Ok(gh) = login.fetch_user(&token).await else {
        return fail(502, "github_unavailable", "could not read your GitHub profile");
    };
    // the admin flag follows configuration on every login, not what the browser says
    let user =
        users::upsert_github(&s.db.pool, gh.id, &gh.login, gh.avatar_url.as_deref(), s.cfg.admin_github_ids.contains(&gh.id)).await?;
    let (sid, max_age) = session::create(&s.db, &user.id, (s.now)()).await?;
    Metrics::inc(&s.metrics.logins);

    let mut r = Redirect::to(&format!("{}{}", s.cfg.web_public_url, safe_next(Some(&st.next)))).into_response();
    r.headers_mut().append(header::SET_COOKIE, session_cookie(&sid, max_age, s.cfg.secure_cookies).parse().expect("ascii"));
    for (k, v) in clear() {
        r.headers_mut().append(k, v.parse().expect("ascii"));
    }
    Ok(r)
}

pub async fn me(State(s): State<AppState>, headers: HeaderMap) -> ApiResult<Json<serde_json::Value>> {
    let a = require_user(&s, &headers, false).await?;
    Ok(Json(json!({
        "id": a.user.id, "login": a.user.github_login, "avatarUrl": a.user.avatar_url, "wallet": a.user.wallet,
        "isAdmin": a.user.is_admin, "csrfToken": csrf_token(&s.cfg.csrf_secret, &a.session_id),
    })))
}

pub async fn logout(State(s): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    let a = require_user(&s, &headers, true).await?;
    session::destroy(&s.db, &a.session_id).await?;
    Ok(([(header::SET_COOKIE, "se_session=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax")], Json(json!({ "ok": true }))).into_response())
}

#[derive(Deserialize)]
pub struct AddressBody {
    address: String,
}

pub async fn wallet_challenge(
    State(s): State<AppState>,
    Extension(ip): Extension<ClientIp>,
    headers: HeaderMap,
    ApiJson(b): ApiJson<AddressBody>,
) -> ApiResult<Json<wallet::Challenge>> {
    let a = require_user(&s, &headers, true).await?;
    mutation_limit(&s, &headers, &ip, 20)?;
    Ok(Json(wallet::create_challenge(&s.db, &a.user.id, &b.address, (s.now)()).await?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyBody {
    challenge_id: String,
    signature: String,
}

pub async fn wallet_verify(
    State(s): State<AppState>,
    Extension(ip): Extension<ClientIp>,
    headers: HeaderMap,
    ApiJson(b): ApiJson<VerifyBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let a = require_user(&s, &headers, true).await?;
    mutation_limit(&s, &headers, &ip, 20)?;
    let address = wallet::verify_challenge(&s.db, &a.user.id, &b.challenge_id, &b.signature, (s.now)()).await?;
    Metrics::inc(&s.metrics.wallets_linked);
    // a payout that was waiting for this wallet continues now; the link itself has already succeeded
    if let Err(e) = on_wallet_linked(&s.payouts, a.user.github_id).await {
        tracing::error!(error = %e, "could not continue pending payouts after a wallet link");
    }
    Ok(Json(json!({ "address": address })))
}

pub async fn wallet_unlink(State(s): State<AppState>, headers: HeaderMap) -> ApiResult<Json<serde_json::Value>> {
    let a = require_user(&s, &headers, true).await?;
    wallet::unlink(&s.db, &a.user.id).await?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn my_earnings(State(s): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    let a = require_user(&s, &headers, false).await?;
    Ok(Json(payouts::earnings(&s.db.pool, a.user.github_id, false).await?).into_response())
}

pub async fn my_funded(State(s): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    let a = require_user(&s, &headers, false).await?;
    let Some(w) = a.user.wallet else {
        return Ok((StatusCode::OK, Json(json!([]))).into_response());
    };
    Ok(Json(bounties::by_maintainer(&s.db.pool, &w).await?).into_response())
}
