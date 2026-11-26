//! The arbiter's tools. Admin rights are checked server-side on every route, and the flag is refreshed from
//! configuration at each login. The arbiter's key never touches the server: these routes only prepare
//! transactions for the arbiter's wallet to sign.

use crate::{
    error::{ApiError, ApiResult},
    extract::{ApiJson, ClientIp},
    routes::{
        auth::mutation_limit,
        prepared_tx,
        public::{Detail, detail_of},
        require_chain_status,
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
use escrow_core::{BountyStatus, PayoutStatus};
use escrow_db::{Dispute, audit, disputes, github, jobs, payouts, webhooks};
use escrow_engine::{indexer::CURSOR_ID, jobs as engine_jobs};
use serde::{Deserialize, Serialize};
use serde_json::json;

async fn admin(s: &AppState, h: &HeaderMap, ip: &ClientIp, mutating: bool) -> ApiResult<Authed> {
    let a = require_user(s, h, mutating).await?;
    if !a.user.is_admin {
        return Err(ApiError::forbidden("forbidden", "admins only"));
    }
    if mutating {
        mutation_limit(s, Some(&a), ip, 30)?;
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
        let overdue = now - d.created_at > Duration::hours(escrow_core::DISPUTE_ALERT_HOURS);
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
    // #134 — building `open_dispute` against a bounty that is no longer Open
    // on chain would fail at simulation with a raw contract error; refuse up
    // front with the real chain status instead.
    require_chain_status(&s, d.bounty_id, BountyStatus::Open).await?;
    let built = s.txb.build_open_dispute(&b.arbiter_address, u64::try_from(d.bounty_id).unwrap_or(0)).await?;
    audit_log(&s, &a, "dispute.open_onchain.build", format!("dispute:{}", d.id), json!({ "bounty": d.bounty_id.to_string() })).await?;
    Ok(Json(prepared_tx(built, None)?))
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
    // #134 — resolution only makes sense while the chain itself considers the
    // bounty Disputed; otherwise the eventual submit would fail at simulation.
    require_chain_status(&s, d.bounty_id, BountyStatus::Disputed).await?;
    let winner = if b.winner_bps > 0 { b.winner.as_deref() } else { None };
    let built = s.txb.build_resolve(&b.arbiter_address, u64::try_from(d.bounty_id).unwrap_or(0), winner, b.winner_bps).await?;
    disputes::set_pending_resolution(&s.db.pool, &d.id, &b.note, winner, b.winner_bps).await?;
    audit_log(&s, &a, "dispute.resolve.build", format!("dispute:{}", d.id), json!({ "winner": b.winner, "bps": b.winner_bps })).await?;
    Ok(Json(prepared_tx(built, None)?))
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
    // The decision and its GitHub comment job commit together: a crash cannot leave one without the other.
    let mut tx = s.db.pool.begin().await?;
    disputes::set_status(&mut *tx, &d.id, "rejected", Some(&b.note)).await?;
    engine_jobs::enqueue_dispute_comment(&mut *tx, d.bounty_id, "rejected", Some(&b.note), &format!("rejected:{}", d.id)).await?;
    tx.commit().await?;
    audit_log(&s, &a, "dispute.rejected", format!("dispute:{}", d.id), json!({ "note": b.note })).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PayoutsQuery {
    status: Option<String>,
    bounty_id: Option<String>,
    cursor: Option<String>,
    limit: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PayoutsPage {
    items: Vec<escrow_db::Payout>,
    next_cursor: Option<String>,
}

fn digits(s: &str, max_len: usize) -> bool {
    !s.is_empty() && s.len() <= max_len && s.bytes().all(|b| b.is_ascii_digit())
}

pub async fn list_payouts(
    State(s): State<AppState>,
    Extension(ip): Extension<ClientIp>,
    headers: HeaderMap,
    q: Result<Query<PayoutsQuery>, axum::extract::rejection::QueryRejection>,
) -> ApiResult<Json<PayoutsPage>> {
    admin(&s, &headers, &ip, false).await?;
    let Query(q) = q.map_err(|_| ApiError::bad_request("invalid query"))?;
    let status = match q.status.as_deref() {
        None => PayoutStatus::Failed,
        Some(s) => s.parse::<PayoutStatus>().map_err(|_| ApiError::bad_request("invalid status"))?,
    };
    let bounty_id = match q.bounty_id.as_deref() {
        None => None,
        Some(v) if digits(v, 18) => Some(v.parse::<i64>().map_err(|_| ApiError::bad_request("invalid bountyId"))?),
        Some(_) => return Err(ApiError::bad_request("invalid bountyId")),
    };
    if q.cursor.as_deref().is_some_and(|c| !id_ok(c)) {
        return Err(ApiError::bad_request("invalid cursor"));
    }
    let limit = match q.limit.as_deref() {
        None => 50,
        Some(l) => l.parse::<i64>().ok().filter(|n| (1..=100).contains(n)).ok_or_else(|| ApiError::bad_request("invalid limit"))?,
    };
    let filter = payouts::ListFilter { status, bounty_id, cursor: q.cursor, limit };
    let mut rows = payouts::list(&s.db.pool, &filter).await?;
    let more = rows.len() as i64 > limit;
    rows.truncate(limit as usize);
    let next_cursor = if more { rows.last().map(|r| r.id.clone()) } else { None };
    Ok(Json(PayoutsPage { items: rows, next_cursor }))
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
    if p.status() != PayoutStatus::Failed {
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

/// #104: reconcile a FAILED payout that already carries a tx hash. Reads the bounty from the
/// chain (authoritative) and settles the payout accordingly: confirmed if the release landed
/// to our winner, requeued if the bounty is still open (the tx expired), otherwise reports
/// what the chain shows so the operator can route to the arbiter or refund path.
#[derive(Serialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileResult {
    /// `confirmed`, `requeued`, or `reported`.
    pub outcome: &'static str,
    /// The bounty's current chain status, verbatim (e.g. "RELEASED", "REFUNDED").
    pub chain_status: String,
    /// The on-chain winner recorded on the bounty, if any.
    pub chain_winner: Option<String>,
    /// The payout's stored tx hash at the time of the reconcile.
    pub tx_hash: String,
}

pub async fn reconcile_payout(
    State(s): State<AppState>,
    Extension(ip): Extension<ClientIp>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<ReconcileResult>> {
    let a = admin(&s, &headers, &ip, true).await?;
    let p = if id_ok(&id) { payouts::get(&s.db.pool, &id).await? } else { None }.ok_or_else(|| ApiError::not_found("payout"))?;
    if p.status != "FAILED" {
        return Err(ApiError::conflict("wrong_state", &format!("payout is {}", p.status)));
    }
    // reconcile is the counterpart to /retry: /retry refuses rows that have a hash and sends
    // them here; this route refuses rows that do not have one and sends them back to /retry.
    let tx_hash = p
        .tx_hash
        .clone()
        .ok_or_else(|| ApiError::conflict("no_tx_hash", "payout has no transaction; use /retry to re-queue it"))?;
    let bounty_u64 = u64::try_from(p.bounty_id).map_err(|_| ApiError::conflict("bad_bounty_id", "bounty id out of range"))?;
    let chain = s.chain.get_bounty(bounty_u64).await?;
    let chain_status = chain.status.to_string();

    let outcome: &'static str = if chain.status == BountyStatus::Released
        && chain.winner.as_deref() == p.winner_address.as_deref()
    {
        // The release did land; just missed the confirmation. Record it and be done.
        payouts::mark_confirmed(&s.db.pool, &p.id, Some(&tx_hash)).await?;
        "confirmed"
    } else if chain.status == BountyStatus::Open {
        // The tx never landed and the bounty is still ours to release. Clear the hash and
        // re-queue so the relayer submits fresh. The guarded UPDATE serializes concurrent
        // reconcile attempts to exactly one winner.
        if !payouts::requeue_failed_with_hash(&s.db.pool, &p.id).await? {
            return Err(ApiError::conflict("wrong_state", "payout changed while reconciling"));
        }
        "requeued"
    } else {
        // Refunded / Disputed / Resolved: nothing to do here; the operator must route it
        // through the arbiter or refund path. The payout stays FAILED and we only report.
        "reported"
    };

    audit_log(
        &s,
        &a,
        "payout.reconcile",
        format!("payout:{}", p.id),
        json!({
            "outcome": outcome,
            "chainStatus": chain_status,
            "chainWinner": chain.winner.clone(),
            "txHash": tx_hash,
        }),
    )
    .await?;

    Ok(Json(ReconcileResult { outcome, chain_status, chain_winner: chain.winner, tx_hash }))
}

#[derive(Deserialize)]
pub struct AiReviewsBody {
    enabled: bool,
}

/// Turns AI reviews (automatic and `/bounty review`) on or off for one tracked repository.
pub async fn set_ai_reviews(
    State(s): State<AppState>,
    Extension(ip): Extension<ClientIp>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    ApiJson(b): ApiJson<AiReviewsBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let a = admin(&s, &headers, &ip, true).await?;
    if github::get_repo(&s.db.pool, id).await?.is_none() {
        return Err(ApiError::not_found("repo"));
    }
    github::set_ai_reviews(&s.db.pool, id, b.enabled).await?;
    audit_log(&s, &a, "repo.ai_reviews", format!("repo:{id}"), json!({ "enabled": b.enabled })).await?;
    Ok(Json(json!({ "ok": true, "aiReviews": b.enabled })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditQuery {
    actor: Option<String>,
    action: Option<String>,
    target_prefix: Option<String>,
    cursor: Option<String>,
    limit: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditRow {
    id: String,
    actor: String,
    action: String,
    target: String,
    data: serde_json::Value,
    created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditPage {
    items: Vec<AuditRow>,
    next_cursor: Option<String>,
}

// #133 — Browsing the audit trail from the API saves investigators from going
// to the database. Rows come back newest-first with keyset pagination over
// (created_at, id); the cursor is the previous page's trailing `id`.
pub async fn list_audit(
    State(s): State<AppState>,
    Extension(ip): Extension<ClientIp>,
    headers: HeaderMap,
    q: Result<Query<AuditQuery>, axum::extract::rejection::QueryRejection>,
) -> ApiResult<Json<AuditPage>> {
    admin(&s, &headers, &ip, false).await?;
    let Query(q) = q.map_err(|_| ApiError::bad_request("invalid query"))?;
    fn short_text(v: &Option<String>, label: &str, max: usize) -> ApiResult<()> {
        if v.as_deref().is_some_and(|s| s.is_empty() || s.chars().count() > max) {
            return Err(ApiError::bad_request(&format!("invalid {label}")));
        }
        Ok(())
    }
    short_text(&q.actor, "actor", 200)?;
    short_text(&q.action, "action", 100)?;
    short_text(&q.target_prefix, "targetPrefix", 200)?;
    if q.cursor.as_deref().is_some_and(|c| !id_ok(c)) {
        return Err(ApiError::bad_request("invalid cursor"));
    }
    let limit = match q.limit.as_deref() {
        None => 50,
        Some(l) => l.parse::<i64>().ok().filter(|n| (1..=100).contains(n)).ok_or_else(|| ApiError::bad_request("invalid limit"))?,
    };
    let filter = audit::Filter {
        actor: q.actor.as_deref(),
        action: q.action.as_deref(),
        target_prefix: q.target_prefix.as_deref(),
        cursor: q.cursor.as_deref(),
        limit: limit + 1,
    };
    let mut rows = audit::list(&s.db.pool, &filter).await?;
    let more = rows.len() as i64 > limit;
    rows.truncate(limit as usize);
    let next_cursor = if more { rows.last().map(|r| r.id.clone()) } else { None };
    let items = rows
        .into_iter()
        .map(|r| AuditRow {
            id: r.id,
            actor: r.actor,
            action: r.action,
            target: r.target,
            data: r.data,
            created_at: r.created_at,
        })
        .collect();
    Ok(Json(AuditPage { items, next_cursor }))
}

pub async fn status(
    State(s): State<AppState>,
    Extension(ip): Extension<ClientIp>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    admin(&s, &headers, &ip, false).await?;
    let cursor = webhooks::get_cursor(&s.db.pool, CURSOR_ID).await?;
    let anomaly_count = audit::count_anomalies_24h(&s.db.pool).await?;
    Ok(Json(json!({
        "indexerLedger": cursor.as_ref().map(|c| c.ledger), "indexerUpdatedAt": cursor.map(|c| c.updated_at),
        "failedPayouts": payouts::count_by_status(&s.db.pool, &[PayoutStatus::Failed]).await?,
        "waitingPayouts": payouts::count_by_status(&s.db.pool, &[PayoutStatus::PendingWallet, PayoutStatus::AwaitingMaintainer]).await?,
        "openDisputes": disputes::count_open(&s.db.pool).await?,
        "deadJobs": jobs::count_dead(&s.db.pool).await?,
        "pendingJobs": jobs::count_pending(&s.db.pool).await?,
        // sign-in exists only with the GitHub App; without it, GitHub jobs are not claimed and wait here
        "waitingForGithub": if s.login.is_some() { 0 } else { escrow_engine::jobs::count_waiting_for_github(&s.db.pool).await? },
        "anomalies24h": anomaly_count,
    })))
}
