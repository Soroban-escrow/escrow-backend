//! Funding links, transaction relay, and the maintainer's own release/refund transactions.
//! The server prepares transactions and relays signed ones; a user's key never touches it.

use crate::{
    error::{ApiError, ApiResult},
    extract::{ApiJson, ClientIp},
    routes::auth::mutation_limit,
    session::require_user,
    state::AppState,
};
use axum::{
    Json,
    extract::{Extension, Path, State},
    http::HeaderMap,
};
use chrono::{DateTime, Utc};
use escrow_chain::{FundParams, keys};
use escrow_core::BountyStatus;
use escrow_db::{bounties, intents, payouts};
use serde::Deserialize;
use serde_json::json;

fn intent_state(i: &escrow_db::FundingIntent, now: DateTime<Utc>) -> &'static str {
    if i.used_at.is_some() {
        "used"
    } else if i.expires_at <= now {
        "expired"
    } else {
        "open"
    }
}

fn intent_id_ok(id: &str) -> bool {
    (10..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub async fn get_intent(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<serde_json::Value>> {
    if !intent_id_ok(&id) {
        return Err(ApiError::not_found("funding link"));
    }
    let i = intents::get(&s.db.pool, &id).await?.ok_or_else(|| ApiError::not_found("funding link"))?;
    let repo = escrow_db::github::get_repo(&s.db.pool, i.repo_id).await?;
    let funded = if i.used_at.is_some() { bounties::latest_for_issue(&s.db.pool, i.repo_id, i.issue_number).await? } else { None };
    Ok(Json(json!({
        "state": intent_state(&i, (s.now)()), "repo": repo.map(|r| r.full_name), "issueNumber": i.issue_number, "token": i.token,
        "amount": i.amount, "durationDays": i.duration_days, "expiresAt": i.expires_at, "bountyId": funded.map(|b| b.id.to_string()),
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildBody {
    address: String,
    relayer_allowed: bool,
}

pub async fn build_fund(
    State(s): State<AppState>,
    Extension(ip): Extension<ClientIp>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<BuildBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let a = require_user(&s, &headers, true).await?;
    mutation_limit(&s, &headers, &ip, 20)?;
    if !keys::is_valid_account(&b.address) {
        return Err(ApiError::bad_request("a valid address and relayerAllowed are required"));
    }
    let i = intents::get(&s.db.pool, &id).await?.ok_or_else(|| ApiError::not_found("funding link"))?;
    let now = (s.now)();
    match intent_state(&i, now) {
        "open" => {}
        other => return Err(ApiError::new(410, &format!("intent_{other}"), &format!("this funding link is {other}"))),
    }
    // only the GitHub user who typed /bounty may fund through this link
    if i.requested_by != a.user.github_id {
        return Err(ApiError::forbidden("not_requester", "this link belongs to another GitHub user"));
    }
    let deadline = u64::try_from(now.timestamp() + i64::from(i.duration_days) * 86_400 + 60).unwrap_or(0);
    let xdr = s
        .txb
        .build_fund(&FundParams {
            maintainer: b.address,
            repo_id: u64::try_from(i.repo_id).unwrap_or(0),
            issue: u64::try_from(i.issue_number).unwrap_or(0),
            token: i.token.clone(),
            amount: i.amount_i128(),
            deadline,
            relayer_allowed: b.relayer_allowed,
        })
        .await?;
    Ok(Json(json!({ "xdr": xdr })))
}

#[derive(Deserialize)]
pub struct SubmitBody {
    xdr: String,
    address: Option<String>,
}

/// Relays a signed single-operation contract call. Never trusts the client's claims about the result.
pub async fn submit(
    State(s): State<AppState>,
    Extension(ip): Extension<ClientIp>,
    headers: HeaderMap,
    ApiJson(b): ApiJson<SubmitBody>,
) -> ApiResult<Json<serde_json::Value>> {
    require_user(&s, &headers, true).await?;
    mutation_limit(&s, &headers, &ip, 20)?;
    if b.xdr.len() < 10 || b.xdr.len() > 20_000 {
        return Err(ApiError::bad_request("xdr required"));
    }
    match s.txb.submit_signed(&b.xdr, b.address.as_deref()).await {
        Ok(hash) => Ok(Json(json!({ "hash": hash }))),
        Err(e) => {
            let msg = match (&e, e.contract_error()) {
                (_, Some(name)) => name.to_string(),
                (escrow_chain::ChainError::Invalid(m), _) => m.clone(),
                (escrow_chain::ChainError::Rejected(m), _) => m.clone(),
                _ if e.is_transient() => return Err(ApiError::from_chain(&e)),
                _ => "rejected".to_string(),
            };
            Err(ApiError::new(422, "submit_rejected", &msg.chars().take(200).collect::<String>()))
        }
    }
}

async fn maintainer_bounty(s: &AppState, headers: &HeaderMap, ip: &ClientIp, id: &str) -> ApiResult<escrow_db::Bounty> {
    let a = require_user(s, headers, true).await?;
    mutation_limit(s, headers, ip, 20)?;
    if id.is_empty() || id.len() > 18 || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ApiError::not_found("bounty"));
    }
    let b = bounties::get(&s.db.pool, id.parse().map_err(|_| ApiError::not_found("bounty"))?)
        .await?
        .ok_or_else(|| ApiError::not_found("bounty"))?;
    if a.user.wallet.as_deref() != Some(b.maintainer.as_str()) {
        return Err(ApiError::forbidden("not_maintainer", "only the maintainer wallet can do this"));
    }
    Ok(b)
}

async fn require_open_on_chain(s: &AppState, id: i64) -> ApiResult<()> {
    // never trust the database for a money decision
    let chain = s.chain.get_bounty(u64::try_from(id).unwrap_or(0)).await?;
    if chain.status != BountyStatus::Open {
        return Err(ApiError::conflict("not_open", &format!("bounty is {}", chain.status)));
    }
    Ok(())
}

pub async fn build_release(
    State(s): State<AppState>,
    Extension(ip): Extension<ClientIp>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let b = maintainer_bounty(&s, &headers, &ip, &id).await?;
    let payout = payouts::for_bounty(&s.db.pool, b.id).await?;
    let (Some(p), true) = (&payout, payout.as_ref().is_some_and(|p| p.status == "AWAITING_MAINTAINER")) else {
        return Err(ApiError::conflict("nothing_to_release", "no payout is waiting for your signature"));
    };
    let winner =
        p.winner_address.clone().ok_or_else(|| ApiError::conflict("nothing_to_release", "no payout is waiting for your signature"))?;
    require_open_on_chain(&s, b.id).await?;
    let xdr = s.txb.build_release(&b.maintainer, u64::try_from(b.id).unwrap_or(0), &winner).await?;
    Ok(Json(json!({ "xdr": xdr, "winner": winner })))
}

pub async fn build_refund(
    State(s): State<AppState>,
    Extension(ip): Extension<ClientIp>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let b = maintainer_bounty(&s, &headers, &ip, &id).await?;
    require_open_on_chain(&s, b.id).await?;
    // the contract enforces deadline + grace itself
    let xdr = s.txb.build_refund(&b.maintainer, u64::try_from(b.id).unwrap_or(0)).await?;
    Ok(Json(json!({ "xdr": xdr })))
}
