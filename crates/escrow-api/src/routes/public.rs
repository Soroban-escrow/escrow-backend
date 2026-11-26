
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
use std::collections::HashMap;
use std::sync::Once;

static METRICS_PUBLIC_WARN: Once = Once::new();

pub async fn health(State(s): State<AppState>) -> Response {
    let h = health_report(&s.db, s.chain.as_ref(), s.cfg.check_indexer_lag).await;
    (
        if h.ok {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(h),
    )
        .into_response()
}

pub async fn metrics(State(s): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    if s.cfg.metrics_token.is_none() {
        METRICS_PUBLIC_WARN.call_once(|| {
            tracing::warn!(
                "METRICS_TOKEN is unset: /metrics is public and exposes bounty, payout and job counts"
            )
        });
    }

    if let Some(token) = &s.cfg.metrics_token {
        let given = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");

        if !ct_eq(given, token) {
            return Err(ApiError::new(
                401,
                "unauthenticated",
                "metrics require a bearer token",
            ));
        }
    }

    Ok((
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        s.metrics.render(&s.db).await,
    )
        .into_response())
}

/// Contract parameters the web app shows (fee, refund grace, durations)
/// and the arbiter address the admin page signs with.
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
        "tokens": {
            "USDC": s.cfg.tokens.usdc,
            "XLM": s.cfg.tokens.xlm
        },
        "explorerTxTemplate": s.cfg.explorer_tx_template,
    });

    Ok(([(header::CACHE_CONTROL, "public, max-age=30")], Json(body)).into_response())
}

pub async fn stats(State(s): State<AppState>) -> ApiResult<Response> {
    let now = (s.now)();
    let mut cache = s.stats_cache.lock().await;
    if let Some(cached) = cache.as_ref()
        && now - cached.cached_at < chrono::Duration::seconds(30)
    {
        return Ok(([(header::CACHE_CONTROL, "public, max-age=30")], Json(cached.data.clone())).into_response());
    }

    let by_status: Vec<(String, i64)> =
        sqlx::query_as("SELECT status, count(*) FROM bounties GROUP BY status")
            .fetch_all(&s.db.pool)
            .await?;

    let open: Vec<(String, i64, String)> = sqlx::query_as(
        "SELECT token, count(*), COALESCE(sum(amount),0)::text
         FROM bounties
         WHERE status = 'OPEN'
         GROUP BY token
         ORDER BY token",
    )
    .fetch_all(&s.db.pool)
    .await?;

    let paid_people: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT winner_github_id)
         FROM payouts
         WHERE status = 'CONFIRMED'",
    )
    .fetch_one(&s.db.pool)
    .await?;

    let mut counts = serde_json::Map::new();

    for st in escrow_core::BountyStatus::ALL {
        counts.insert(
            st.as_str().into(),
            json!(
                by_status
                    .iter()
                    .find(|(k, _)| k == st.as_str())
                    .map_or(0, |(_, n)| *n)
            ),
        );
    }

    let open_by_token: Vec<_> = open
        .into_iter()
        .map(|(t, n, amount)| {
            json!({
                "token": t,
                "symbol": s.cfg.tokens.symbol_for(&t),
                "count": n,
                "amount": amount
            })
        })
        .collect();
    let data = json!({
        "bounties": counts,
        "openByToken": open_by_token,
        "contributorsPaid": paid_people
    });
    *cache = Some(crate::state::StatsCache { data: data.clone(), cached_at: now });

    Ok((
        [(header::CACHE_CONTROL, "public, max-age=30")],
        Json(data),
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
        Some(l) => l
            .parse::<i64>()
            .ok()
            .filter(|n| (1..=50).contains(n))
            .ok_or_else(bad)?,
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

    let next_cursor = if more {
        rows.last().map(|r| r.bounty.id.to_string())
    } else {
        None
    };

    Ok(Json(Page {
        items: rows,
        next_cursor,
    }))
}

#[derive(Serialize, Clone)]
pub struct PrView {
    #[serde(flatten)]
    pr: PullRequest,
    reviews: Vec<AiReview>,
}

#[derive(Serialize, Clone)]
pub struct Detail {
    #[serde(flatten)]
    pub bounty: Bounty,
    pub repo: RepoName,
    pub prs: Vec<PrView>,
    pub payouts: Vec<Payout>,
}

/// Batch-load bounty details for a collection of bounty IDs.
///
/// This function is intended to replace repeated `detail_of()` calls when
/// assembling admin dispute pages.
///
pub async fn detail_of_many(
    s: &AppState,
    ids: &[i64],
) -> ApiResult<HashMap<i64, Option<Detail>>> {
    let mut result = HashMap::with_capacity(ids.len());

    if ids.is_empty() {
        return Ok(result);
    }

    // Batch fetch bounties and repositories.
    let bounty_rows = bounties::with_repo_many(&s.db.pool, ids).await?;

    // Batch fetch all PRs associated with these bounties.
    let pr_rows = prs::for_bounties(&s.db.pool, ids).await?;

    // Collect PR IDs for the batch review query.
    let pr_ids: Vec<i64> = pr_rows.iter().map(|(_, pr)| pr.id).collect();

    // Batch fetch all AI reviews for the collected PRs.
    let review_rows = reviews::latest_for_prs(&s.db.pool, &pr_ids).await?;

    // Batch fetch payouts associated with all bounties.
    let payout_rows = payouts::all_for_bounties(&s.db.pool, ids).await?;

    // Group PRs by bounty ID.
    let mut prs_by_bounty: HashMap<i64, Vec<PullRequest>> = HashMap::new();
    for (bounty_id, pr) in pr_rows {
        prs_by_bounty.entry(bounty_id).or_default().push(pr);
    }

    // Group reviews by PR ID.
    let mut reviews_by_pr: HashMap<_, Vec<AiReview>> = HashMap::new();
    for review in review_rows {
        reviews_by_pr
            .entry(review.pr_id)
            .or_default()
            .push(review);
    }

    // Group payouts by bounty ID.
    let mut payouts_by_bounty: HashMap<i64, Vec<Payout>> = HashMap::new();
    for payout in payout_rows {
        payouts_by_bounty
            .entry(payout.bounty_id)
            .or_default()
            .push(payout);
    }

    // Assemble details.
    for (bounty, repo) in bounty_rows {
        let bounty_id = bounty.id;

        let prs = prs_by_bounty
            .remove(&bounty_id)
            .unwrap_or_default()
            .into_iter()
            .map(|pr| {
                let reviews = reviews_by_pr.remove(&pr.id).unwrap_or_default();
                PrView { pr, reviews }
            })
            .collect();

        let payouts = payouts_by_bounty.remove(&bounty_id).unwrap_or_default();

        result.insert(
            bounty_id,
            Some(Detail {
                bounty,
                repo: RepoName {
                    full_name: repo.full_name,
                },
                prs,
                payouts,
            }),
        );
    }

    // Missing bounty records are represented as None to preserve the optional
    // detail contract expected by callers.
    for id in ids {
        result.entry(*id).or_insert(None);
    }

    Ok(result)
}

/// Loads details for one bounty using the batch loader.
///
/// Keeping a single implementation path ensures single-bounty and batch
/// responses are assembled consistently.
pub async fn detail_of(s: &AppState, id: i64) -> ApiResult<Option<Detail>> {
    let details = detail_of_many(s, &[id]).await?;
    Ok(details.get(&id).cloned().flatten())
}

pub async fn get_bounty(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Detail>> {
    if !digits(&id, 18) {
        return Err(ApiError::not_found("bounty"));
    }

    detail_of(&s, id.parse().map_err(|_| ApiError::not_found("bounty"))?)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("bounty"))
}

/// `limit` and `cursor` for the earnings lists (`/me/earnings` and a public
/// profile's `paid`).
#[derive(Deserialize, Default)]
pub struct EarningsQuery {
    cursor: Option<String>,
    limit: Option<String>,
}

/// A page of payouts as `/bounties` pages: the rows and the cursor for the
/// next page, if any.
pub(crate) struct EarningsPage {
    pub items: Vec<payouts::Earning>,
    pub next_cursor: Option<String>,
}

/// Loads one page of `github_id`'s payouts, validating `limit` (1–50,
/// default 20) and `cursor` (a payout id).
pub(crate) async fn earnings_page(
    s: &AppState,
    github_id: i64,
    only_confirmed: bool,
    q: Result<Query<EarningsQuery>, axum::extract::rejection::QueryRejection>,
) -> ApiResult<EarningsPage> {
    let Query(q) = q.map_err(|_| ApiError::bad_request("invalid query"))?;
    let bad = || ApiError::bad_request("invalid query");

    if q.cursor.as_deref().is_some_and(|c| {
        !(10..=40).contains(&c.len())
            || !c.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    }) {
        return Err(bad());
    }

    let limit = match q.limit.as_deref() {
        None => 20,
        Some(l) => l
            .parse::<i64>()
            .ok()
            .filter(|n| (1..=50).contains(n))
            .ok_or_else(bad)?,
    };

    let mut items = payouts::earnings(
        &s.db.pool,
        github_id,
        only_confirmed,
        q.cursor.as_deref(),
        limit,
    )
    .await?;

    let more = items.len() as i64 > limit;
    items.truncate(limit as usize);

    let next_cursor = if more {
        items.last().map(|e| e.payout.id.clone())
    } else {
        None
    };

    Ok(EarningsPage {
        items,
        next_cursor,
    })
}

pub async fn get_user(
    State(s): State<AppState>,
    Path(login): Path<String>,
    q: Result<Query<EarningsQuery>, axum::extract::rejection::QueryRejection>,
) -> ApiResult<Json<serde_json::Value>> {
    if login.len() > 39 || !login.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(ApiError::not_found("user"));
    }

    let u = users::by_login(&s.db.pool, &login)
        .await?
        .ok_or_else(|| ApiError::not_found("user"))?;

    let page = earnings_page(&s, u.github_id, true, q).await?;

    let paid: Vec<_> = page
        .items
        .into_iter()
        .map(|e| json!({ "bounty": e.bounty, "txHash": e.payout.tx_hash }))
        .collect();

    Ok(Json(json!({
        "login": u.github_login,
        "avatarUrl": u.avatar_url,
        "paid": paid,
        "nextCursor": page.next_cursor
    })))
}
