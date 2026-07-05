//! The arbiter's tools. Admin rights are checked server-side on every route, and the flag is refreshed from
//! configuration at each login. The arbiter's key never touches the server: these routes only prepare
//! transactions for the arbiter's wallet to sign.

use crate::{
    error::{ApiError, ApiResult},
    extract::{ApiJson, ClientIp},
    routes::{
        auth::mutation_limit,
        public::{Detail, detail_of},
    },
    session::{Authed, require_user},
    state::AppState,
};
use axum::{
    Json,
    extract::{Extension, Path, Query, State},
    http::HeaderMap,
};
use chrono::Duration;
use escrow_chain::keys;
use escrow_db::{Dispute, audit, disputes, jobs, payouts, webhooks};
use escrow_engine::indexer::CURSOR_ID;
use serde::{Deserialize, Serialize};
use serde_json::json;

const OVERDUE_HOURS: i64 = 36;

async fn admin(s: &AppState, h: &HeaderMap, ip: &ClientIp, mutating: bool) -> ApiResult<Authed> {
    let a = require_user(s, h, mutating).await?;
    if !a.user.is_admin {
        return Err(ApiError::forbidden("forbidden", "admins only"));
    }
    if mutating {
        mutation_limit(s, h, ip, 30)?;
    }
    Ok(a)
}

fn id_ok(id: &str) -> bool {
    (10..=40).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

async fn load(s: &AppState, id: &str) -> ApiResult<Dispute> {
    if !id_ok(id) {
        return Err(ApiError::not_found("dispute"));
    }
    disputes::get(&s.db.pool, id).await?.ok_or_else(|| ApiError::not_found("dispute"))
}

async fn audit_log(s: &AppState, a: &Authed, action: &str, target: String, data: serde_json::Value) -> ApiResult<()> {
    Ok(audit::log(&s.db.pool, &format!("admin:{}", a.user.github_id), action, &target, &data).await?)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisputeView {
    #[serde(flatten)]
    dispute: Dispute,
    bounty: Option<Detail>,
    overdue: bool,
}

pub async fn list_disputes(
    State(s): State<AppState>,
    Extension(ip): Extension<ClientIp>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<DisputeView>>> {
    admin(&s, &headers, &ip, false).await?;
    let now = (s.now)();
    let mut out = vec![];
    for d in disputes::list_open(&s.db.pool).await? {
        let overdue = now - d.created_at > Duration::hours(OVERDUE_HOURS);
        out.push(DisputeView { bounty: detail_of(&s, d.bounty_id).await?, dispute: d, overdue });
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArbiterBody {
    arbiter_address: String,
}

fn valid_account(a: &str) -> ApiResult<()> {
    keys::is_valid_account(a).then_some(()).ok_or_else(|| ApiError::bad_request("a valid arbiterAddress is required"))
}

pub async fn open_onchain(
    State(s): State<AppState>,
    Extension(ip): Extension<ClientIp>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<ArbiterBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let a = admin(&s, &headers, &ip, true).await?;
    let d = load(&s, &id).await?;
    valid_account(&b.arbiter_address)?;
    if d.status != "requested" {
        return Err(ApiError::conflict("wrong_state", &format!("dispute is {}", d.status)));
    }
    let xdr = s.txb.build_open_dispute(&b.arbiter_address, u64::try_from(d.bounty_id).unwrap_or(0)).await?;
    audit_log(&s, &a, "dispute.open_onchain.build", format!("dispute:{}", d.id), json!({ "bounty": d.bounty_id.to_string() })).await?;
    Ok(Json(json!({ "xdr": xdr })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveBody {
    arbiter_address: String,
    winner: Option<String>,
    winner_bps: u32,
    #[serde(default)]
    note: String,
}

pub async fn resolve_build(
    State(s): State<AppState>,
    Extension(ip): Extension<ClientIp>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<ResolveBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let a = admin(&s, &headers, &ip, true).await?;
    let d = load(&s, &id).await?;
    valid_account(&b.arbiter_address)?;
    if b.winner_bps > 10_000 || b.note.chars().count() > 1000 || b.winner.as_deref().is_some_and(|w| !keys::is_valid_account(w)) {
        return Err(ApiError::bad_request("arbiterAddress, winnerBps (0-10000) and, when bps > 0, a winner are required"));
    }
    if b.winner_bps > 0 && b.winner.is_none() {
        return Err(ApiError::bad_request("a winner address is required when winnerBps > 0").with_code("winner_required"));
    }
    if d.status != "opened_onchain" {
        return Err(ApiError::conflict("wrong_state", &format!("dispute is {}; open it on chain first", d.status)));
    }
    let winner = if b.winner_bps > 0 { b.winner.as_deref() } else { None };
    let xdr = s.txb.build_resolve(&b.arbiter_address, u64::try_from(d.bounty_id).unwrap_or(0), winner, b.winner_bps).await?;
    disputes::set_note(&s.db.pool, &d.id, &b.note).await?;
    audit_log(&s, &a, "dispute.resolve.build", format!("dispute:{}", d.id), json!({ "winner": b.winner, "bps": b.winner_bps })).await?;
    Ok(Json(json!({ "xdr": xdr })))
}

#[derive(Deserialize)]
pub struct RejectBody {
    note: String,
}

pub async fn reject(
    State(s): State<AppState>,
    Extension(ip): Extension<ClientIp>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ApiJson(b): ApiJson<RejectBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let a = admin(&s, &headers, &ip, true).await?;
    let d = load(&s, &id).await?;
    if b.note.is_empty() || b.note.chars().count() > 1000 {
        return Err(ApiError::bad_request("a note is required"));
    }
    if d.status != "requested" {
        return Err(ApiError::conflict("wrong_state", &format!("dispute is {}", d.status)));
    }
    disputes::set_status(&s.db.pool, &d.id, "rejected", Some(&b.note)).await?;
    audit_log(&s, &a, "dispute.rejected", format!("dispute:{}", d.id), json!({ "note": b.note })).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct StatusQuery {
    status: Option<String>,
}

pub async fn list_payouts(
    State(s): State<AppState>,
    Extension(ip): Extension<ClientIp>,
    headers: HeaderMap,
    Query(q): Query<StatusQuery>,
) -> ApiResult<Json<Vec<escrow_db::Payout>>> {
    admin(&s, &headers, &ip, false).await?;
    let status = q.status.unwrap_or_else(|| "FAILED".into());
    if status.parse::<escrow_core::PayoutStatus>().is_err() {
        return Err(ApiError::bad_request("invalid status"));
    }
    Ok(Json(payouts::by_status(&s.db.pool, &status, 100).await?))
}

/// Only payouts with no submitted transaction can be re-queued; one with a hash must be checked on chain first.
pub async fn retry_payout(
    State(s): State<AppState>,
    Extension(ip): Extension<ClientIp>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let a = admin(&s, &headers, &ip, true).await?;
    let p = if id_ok(&id) { payouts::get(&s.db.pool, &id).await? } else { None }.ok_or_else(|| ApiError::not_found("payout"))?;
    if p.status != "FAILED" {
        return Err(ApiError::conflict("wrong_state", &format!("payout is {}", p.status)));
    }
    if p.tx_hash.is_some() {
        return Err(ApiError::conflict("has_tx_hash", "a transaction was already submitted; verify it on chain instead of retrying"));
    }
    if !payouts::retry_failed(&s.db.pool, &p.id).await? {
        return Err(ApiError::conflict("wrong_state", "payout changed while retrying"));
    }
    audit_log(&s, &a, "payout.retry", format!("payout:{}", p.id), json!({})).await?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn status(
    State(s): State<AppState>,
    Extension(ip): Extension<ClientIp>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    admin(&s, &headers, &ip, false).await?;
    let cursor = webhooks::get_cursor(&s.db.pool, CURSOR_ID).await?;
    Ok(Json(json!({
        "indexerLedger": cursor.as_ref().map(|c| c.ledger), "indexerUpdatedAt": cursor.map(|c| c.updated_at),
        "failedPayouts": payouts::count_by_status(&s.db.pool, &["FAILED"]).await?,
        "waitingPayouts": payouts::count_by_status(&s.db.pool, &["PENDING_WALLET", "AWAITING_MAINTAINER"]).await?,
        "openDisputes": disputes::count_open(&s.db.pool).await?,
        "deadJobs": jobs::count_dead(&s.db.pool).await?,
        "pendingJobs": jobs::count_pending(&s.db.pool).await?,
    })))
}
