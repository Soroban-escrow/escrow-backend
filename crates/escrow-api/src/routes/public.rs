//! Unauthenticated reads: health, metrics, configuration, statistics, bounties and public profiles.

use crate::{
    error::{ApiError, ApiResult},
    state::AppState,
};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use escrow_core::security::ct_eq;
use escrow_db::{
    AiReview, Bounty, Payout, PullRequest,
    bounties::{self, ListFilter, Listed, RepoName, Sort},
    payouts, prs, reviews, users,
};
use escrow_engine::ops::health_report;
use serde::{Deserialize, Serialize};
use serde_json::json;

pub async fn health(State(s): State<AppState>) -> Response {
    let h = health_report(&s.db, s.chain.as_ref()).await;
    (if h.ok { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE }, Json(h)).into_response()
}

pub async fn metrics(State(s): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    if let Some(token) = &s.cfg.metrics_token {
        let given = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
        if !ct_eq(given, token) {
            return Err(ApiError::new(401, "unauthenticated", "metrics require a bearer token"));
        }
    }
    Ok(([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], s.metrics.render(&s.db).await).into_response())
}

/// Contract parameters the web app shows (fee, refund grace, durations) and the arbiter address the admin page signs with.
pub async fn config(State(s): State<AppState>) -> ApiResult<Response> {
    let c = s.chain.get_config().await?;
    let body = json!({
        "network": s.cfg.network,
        "feeBps": c.fee_bps,
        "graceSecs": c.grace_secs,
        "minDurationSecs": c.min_duration_secs,
        "maxDurationSecs": c.max_duration_secs,
        "paused": c.paused,
        "arbiter": c.arbiter,
        "relayer": c.relayer,
        "treasury": c.treasury,
        "tokens": { "USDC": s.cfg.tokens.usdc, "XLM": s.cfg.tokens.xlm },
        "explorerTxTemplate": s.cfg.explorer_tx_template,
    });
    Ok(([(header::CACHE_CONTROL, "public, max-age=30")], Json(body)).into_response())
}

pub async fn stats(State(s): State<AppState>) -> ApiResult<Response> {
    let by_status: Vec<(String, i64)> =
        sqlx::query_as("SELECT status, count(*) FROM bounties GROUP BY status").fetch_all(&s.db.pool).await?;
    let open: Vec<(String, i64, String)> = sqlx::query_as(
        "SELECT token, count(*), COALESCE(sum(amount),0)::text FROM bounties WHERE status = 'OPEN' GROUP BY token ORDER BY token",
    )
    .fetch_all(&s.db.pool)
    .await?;
    let paid_people: i64 =
        sqlx::query_scalar("SELECT count(DISTINCT winner_github_id) FROM payouts WHERE status = 'CONFIRMED'").fetch_one(&s.db.pool).await?;
    let mut counts = serde_json::Map::new();
    for st in escrow_core::BountyStatus::ALL {
        counts.insert(st.as_str().into(), json!(by_status.iter().find(|(k, _)| k == st.as_str()).map_or(0, |(_, n)| *n)));
    }
    let open_by_token: Vec<_> = open
        .into_iter()
        .map(|(t, n, amount)| json!({ "token": t, "symbol": s.cfg.tokens.symbol_for(&t), "count": n, "amount": amount }))
        .collect();
    Ok((
        [(header::CACHE_CONTROL, "public, max-age=30")],
        Json(json!({ "bounties": counts, "openByToken": open_by_token, "contributorsPaid": paid_people })),
    )
        .into_response())
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ListQuery {
    status: Option<String>,
    token: Option<String>,
    min_amount: Option<String>,
    q: Option<String>,
    sort: Option<String>,
    cursor: Option<String>,
    limit: Option<String>,
}

fn digits(s: &str, max_len: usize) -> bool {
    !s.is_empty() && s.len() <= max_len && s.bytes().all(|b| b.is_ascii_digit())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    items: Vec<Listed>,
    next_cursor: Option<String>,
}

pub async fn list_bounties(
    State(s): State<AppState>,
    q: Result<Query<ListQuery>, axum::extract::rejection::QueryRejection>,
) -> ApiResult<Json<Page>> {
    let Query(q) = q.map_err(|_| ApiError::bad_request("invalid query"))?;
    let bad = || ApiError::bad_request("invalid query");
    let status = match q.status.as_deref() {
        None => "OPEN".to_string(),
        Some(v) if v.parse::<escrow_core::BountyStatus>().is_ok() => v.to_string(),
        Some(_) => return Err(bad()),
    };
    let sort = match q.sort.as_deref() {
        None | Some("newest") => Sort::Newest,
        Some("highest") => Sort::Highest,
        Some("ending") => Sort::Ending,
        Some(_) => return Err(bad()),
    };
    if q.min_amount.as_deref().is_some_and(|m| !digits(m, 39))
        || q.cursor.as_deref().is_some_and(|c| !digits(c, 18))
        || q.q.as_deref().is_some_and(|t| t.chars().count() > 100)
    {
        return Err(bad());
    }
    let limit = match q.limit.as_deref() {
        None => 20,
        Some(l) => l.parse::<i64>().ok().filter(|n| (1..=50).contains(n)).ok_or_else(bad)?,
    };
    let filter = ListFilter {
        status,
        token: q.token,
        min_amount: q.min_amount,
        q: q.q.filter(|t| !t.is_empty()),
        sort,
        cursor: q.cursor.and_then(|c| c.parse().ok()),
        limit,
    };
    let mut rows = bounties::list(&s.db.pool, &filter).await?;
    let more = rows.len() as i64 > limit;
    rows.truncate(limit as usize);
    let next_cursor = if more { rows.last().map(|r| r.bounty.id.to_string()) } else { None };
    Ok(Json(Page { items: rows, next_cursor }))
}

#[derive(Serialize)]
pub struct PrView {
    #[serde(flatten)]
    pr: PullRequest,
    reviews: Vec<AiReview>,
}

#[derive(Serialize)]
pub struct Detail {
    #[serde(flatten)]
    pub bounty: Bounty,
    pub repo: RepoName,
    pub prs: Vec<PrView>,
    pub payouts: Vec<Payout>,
}

pub async fn detail_of(s: &AppState, id: i64) -> ApiResult<Option<Detail>> {
    let Some((bounty, repo)) = bounties::with_repo(&s.db.pool, id).await? else {
        return Ok(None);
    };
    let mut views = vec![];
    for pr in prs::for_bounty(&s.db.pool, id).await? {
        let reviews = reviews::latest_for_pr(&s.db.pool, pr.id).await?;
        views.push(PrView { pr, reviews });
    }
    let payouts = payouts::all_for_bounty(&s.db.pool, id).await?;
    Ok(Some(Detail { bounty, repo: RepoName { full_name: repo.full_name }, prs: views, payouts }))
}

pub async fn get_bounty(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Detail>> {
    if !digits(&id, 18) {
        return Err(ApiError::not_found("bounty"));
    }
    detail_of(&s, id.parse().map_err(|_| ApiError::not_found("bounty"))?).await?.map(Json).ok_or_else(|| ApiError::not_found("bounty"))
}

pub async fn get_user(State(s): State<AppState>, Path(login): Path<String>) -> ApiResult<Json<serde_json::Value>> {
    if login.len() > 39 || !login.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(ApiError::not_found("user"));
    }
    let u = users::by_login(&s.db.pool, &login).await?.ok_or_else(|| ApiError::not_found("user"))?;
    let paid: Vec<_> = payouts::earnings(&s.db.pool, u.github_id, true)
        .await?
        .into_iter()
        .map(|e| json!({ "bounty": e.bounty, "txHash": e.payout.tx_hash }))
        .collect();
    Ok(Json(json!({ "login": u.github_login, "avatarUrl": u.avatar_url, "paid": paid })))
}
