//! The indexer folds contract events into the database. It is the ONLY writer of bounty status:
//! no HTTP handler and no webhook can change whether a bounty is open, paid or refunded. A lost
//! database is rebuilt by re-indexing; a wrong database is corrected by the next event.
//!
//! Each page of events and the cursor that follows it are committed in ONE transaction, so a crash
//! replays at most one page, and replaying is harmless: every bounty remembers the last event it
//! applied, and an older event is recognised as stale instead of being applied twice.

use crate::{error::EngineError, jobs};
use chrono::{DateTime, Utc};
use escrow_chain::{ChainError, ChainEvent, ChainReader, EventSource, EventsPage, events::cursor_ledger};
use escrow_db::{Db, audit, bounties, disputes, intents, payouts, untracked, users, webhooks};
use serde_json::json;
use sqlx::PgConnection;
use std::{sync::Arc, time::Duration as StdDuration};
use tokio_util::sync::CancellationToken;

pub const CURSOR_ID: &str = "bounty_escrow";
pub const PAGE_LIMIT: u32 = 200;
pub const MAX_PAGES: usize = 200;
/// After this many consecutive poll failures, an alert is raised.
pub const FAILURE_ALERT_THRESHOLD: u32 = 5;
/// Maximum backoff between retries when failing repeatedly.
const MAX_BACKOFF_SECS: u64 = 60;

#[derive(Clone)]
pub struct IndexerDeps {
    pub db: Db,
    pub source: Arc<dyn EventSource>,
    pub chain: Arc<dyn ChainReader>,
    /// where to start on a brand-new database (the contract's deployment ledger is ideal)
    pub start_ledger: Option<u32>,
    /// otherwise start this many ledgers behind the tip
    pub lookback: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Applied,
    /// an older (or repeated) event; already reflected
    Stale,
    /// not ours: someone funded a repository we do not track
    Untracked,
    /// contradicts what we know (e.g. a release for a bounty that is not open); ignored and logged
    Anomaly,
}

pub struct EventMeta<'a> {
    pub event_id: &'a str,
    pub tx_hash: &'a str,
    pub closed_at: DateTime<Utc>,
    /// Ledger the event was emitted in, stored on untracked events so a later
    /// reindex knows where to start.
    pub ledger: u32,
}

/// Applies one event inside the caller's transaction. Idempotent. Returns what happened so the caller
/// enqueues follow-up work only for `Applied`. A malformed tracked event is a decode error: the page
/// aborts and nothing is indexed, never something made up.
pub async fn apply_event(conn: &mut PgConnection, ev: &ChainEvent, meta: &EventMeta<'_>) -> Result<Outcome, EngineError> {
    let id = i64::try_from(ev.bounty_id()).map_err(|_| {
        EngineError::Chain(ChainError::Decode(format!("event {}: bounty id {} is out of range", meta.event_id, ev.bounty_id())))
    })?;
    if let ChainEvent::Funded { repo_id, issue, token, amount, fee_bps, deadline, relayer_allowed, maintainer, .. } = ev {
        let repo_id = i64::try_from(*repo_id).map_err(|_| {
            EngineError::Chain(ChainError::Decode(format!("event {}: repo id {} is out of range", meta.event_id, *repo_id)))
        })?;
        // anyone can fund any repository on chain; we only track the ones with our App installed
        let Some(repo) = escrow_db::github::get_repo(&mut *conn, repo_id).await? else {
            // Store the untracked funded event so it can be applied later when the repo is installed
            untracked::store_untracked_funded(
                &mut *conn,
                meta.event_id,
                ev.bounty_id(),
                u64::try_from(repo_id).unwrap_or(0),
                *issue,
                maintainer,
                token,
                *amount,
                *fee_bps,
                DateTime::from_timestamp(i64::try_from(*deadline).unwrap_or(0), 0).unwrap_or(meta.closed_at),
                *relayer_allowed,
                meta.ledger,
                meta.tx_hash,
                meta.closed_at,
            )
            .await?;
            return Ok(Outcome::Untracked);
        };
        if let Some(existing) = bounties::get(&mut *conn, id).await? {
            return Ok(if existing.last_event_id.as_deref().is_some_and(|l| l >= meta.event_id) {
                Outcome::Stale
            } else {
                Outcome::Anomaly
            });
        }
        let issue_number = i32::try_from(*issue)
            .map_err(|_| EngineError::Chain(ChainError::Decode(format!("event {}: issue {} is out of range", meta.event_id, *issue))))?;
        let intent = intents::find_unused_match(&mut *conn, repo_id, issue_number, token, *amount).await?;
        if let Some(i) = &intent {
            intents::mark_used(&mut *conn, &i.id, meta.closed_at).await?;
        }
        // With a funding link the maintainer is whoever requested it. Funded straight on chain there is no
        // intent, so fall back to the account that has linked the maintainer wallet (it may still be unset,
        // and is backfilled by `bounties::backfill_maintainer_github_id` when that wallet is later linked).
        let maintainer_github_id = match &intent {
            Some(i) => Some(i.requested_by),
            None => users::by_wallet(&mut *conn, maintainer).await?.map(|u| u.github_id),
        };
        bounties::insert(
            &mut *conn,
            &bounties::NewBounty {
                id,
                repo_id,
                issue_number,
                issue_url: &format!("https://github.com/{}/issues/{issue_number}", repo.full_name),
                maintainer,
                maintainer_github_id,
                token,
                amount: *amount,
                fee_bps: i32::try_from(*fee_bps).map_err(|_| {
                    EngineError::Chain(ChainError::Decode(format!("event {}: fee_bps {} is out of range", meta.event_id, *fee_bps)))
                })?,
                deadline: {
                    let ts = i64::try_from(*deadline).map_err(|_| {
                        EngineError::Chain(ChainError::Decode(format!("event {}: deadline {} is out of range", meta.event_id, *deadline)))
                    })?;
                    DateTime::from_timestamp(ts, 0).ok_or_else(|| {
                        EngineError::Chain(ChainError::Decode(format!(
                            "event {}: deadline {} is not a valid timestamp",
                            meta.event_id, *deadline
                        )))
                    })?
                },
                relayer_allowed: *relayer_allowed,
                fund_tx: meta.tx_hash,
                created_at: meta.closed_at,
                last_event_id: meta.event_id,
            },
        )
        .await?;
        // Clean up any untracked event for this bounty now that it's tracked
        untracked::delete_untracked_by_bounty(&mut *conn, id).await?;
        return Ok(Outcome::Applied);
    }

    let Some(b) = bounties::get_for_update(&mut *conn, id).await? else {
        return Ok(Outcome::Untracked);
    };
    if b.last_event_id.as_deref().is_some_and(|l| l >= meta.event_id) {
        return Ok(Outcome::Stale);
    }
    let status = b.status();
    match ev {
        ChainEvent::Released { winner, .. } => {
            if status != escrow_core::BountyStatus::Open {
                return Ok(Outcome::Anomaly);
            }
            let winner_user = users::by_wallet(&mut *conn, winner).await?.map(|u| u.github_id);
            bounties::apply_released(&mut *conn, id, meta.event_id, meta.tx_hash, winner, winner_user).await?;
            payouts::confirm_from_chain(&mut *conn, id, meta.tx_hash, winner).await?;
        }
        ChainEvent::Refunded { .. } => {
            if status != escrow_core::BountyStatus::Open {
                return Ok(Outcome::Anomaly);
            }
            bounties::apply_refunded(&mut *conn, id, meta.event_id, meta.tx_hash).await?;
        }
        ChainEvent::Disputed { .. } => {
            if status != escrow_core::BountyStatus::Open {
                return Ok(Outcome::Anomaly);
            }
            bounties::apply_disputed(&mut *conn, id, meta.event_id).await?;
            disputes::mark_opened_onchain(&mut *conn, id).await?;
        }
        ChainEvent::Resolved { winner, .. } => {
            if status != escrow_core::BountyStatus::Disputed {
                return Ok(Outcome::Anomaly);
            }
            bounties::apply_resolved(&mut *conn, id, meta.event_id, meta.tx_hash, winner.as_deref()).await?;
            disputes::mark_resolved(&mut *conn, id).await?;
        }
        ChainEvent::Funded { .. } => unreachable!("handled above"),
    }
    Ok(Outcome::Applied)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct PollReport {
    pub events_seen: usize,
    pub applied: usize,
    pub anomalies: usize,
    /// the saved cursor was outside the RPC's retention; it was dropped and the next poll restarts from the fallback
    pub cursor_reset: bool,
    /// true when the poll caught up to the chain tip; false when it stopped because MAX_PAGES was reached
    pub caught_up: bool,
}

async fn fetch(d: &IndexerDeps, saved: Option<&escrow_db::IndexerCursor>) -> Result<EventsPage, ChainError> {
    match saved.and_then(|s| s.cursor.as_deref()) {
        Some(c) => d.source.events_from_cursor(c, PAGE_LIMIT).await,
        None => {
            let start = match d.start_ledger {
                Some(s) => s,
                None => d.chain.latest_ledger().await?.saturating_sub(d.lookback),
            };
            d.source.events_from_ledger(start, PAGE_LIMIT).await
        }
    }
}

/// Catches up to the chain tip. The RPC scans a bounded ledger range per request and may answer a quiet
/// range with an empty page and an advanced cursor, so we keep paging until the cursor reaches the tip.
pub async fn poll_once(d: &IndexerDeps) -> Result<PollReport, EngineError> {
    let mut report = PollReport::default();
    for _ in 0..MAX_PAGES {
        let saved = webhooks::get_cursor(&d.db.pool, CURSOR_ID).await?;
        let page = match fetch(d, saved.as_ref()).await {
            Ok(p) => p,
            Err(e) if e.is_out_of_retention() => {
                tracing::warn!(error = %e, "cursor outside the retained range; restarting from the fallback");
                webhooks::delete_cursor(&d.db.pool, CURSOR_ID).await?;
                report.cursor_reset = true;
                return Ok(report);
            }
            Err(e) => return Err(e.into()),
        };

        let mut tx = d.db.pool.begin().await?;
        // follow the cursor, not just events: a quiet contract must still show an up-to-date ledger (health, lag alerts)
        let mut ledger = saved.as_ref().map_or(0, |s| s.ledger.max(0) as u32).max(cursor_ledger(&page.cursor));
        for raw in &page.events {
            if !raw.in_successful_contract_call {
                continue;
            }
            // a malformed tracked event aborts the page: stop and alert rather than index something wrong
            let decoded = raw.decode()?;
            ledger = ledger.max(raw.ledger);
            let Some(ev) = decoded else { continue };
            let closed_at = match DateTime::parse_from_rfc3339(&raw.ledger_closed_at) {
                Ok(t) => t.with_timezone(&Utc),
                Err(e) => {
                    tracing::warn!(event = %raw.id, value = %raw.ledger_closed_at, error = %e, "ledger_closed_at could not be parsed; falling back to now");
                    Utc::now()
                }
            };
            let meta = EventMeta { event_id: &raw.id, tx_hash: &raw.tx_hash, closed_at, ledger: raw.ledger };
            match apply_event(&mut tx, &ev, &meta).await? {
                Outcome::Applied => {
                    report.applied += 1;
                    let bid = i64::try_from(ev.bounty_id()).unwrap_or(0);
                    jobs::enqueue_card_refresh(&mut *tx, bid).await?;
                    if matches!(ev, ChainEvent::Released { .. }) {
                        jobs::enqueue_payout_announcement(&mut *tx, bid).await?;
                    }
                }
                Outcome::Anomaly => {
                    report.anomalies += 1;
                    // Fetch the stored status for the audit log
                    let stored_status = bounties::get(&mut *tx, i64::try_from(ev.bounty_id()).unwrap_or(0))
                        .await?
                        .map(|b| b.status().to_string())
                        .unwrap_or_else(|| "UNKNOWN".to_string());
                    // Persist anomaly to audit log for admin visibility
                    audit::log(
                        &mut *tx,
                        "indexer",
                        "anomaly_detected",
                        &format!("bounty:{}", ev.bounty_id()),
                        &json!({
                            "eventId": raw.id,
                            "kind": ev.kind(),
                            "bountyId": ev.bounty_id(),
                            "storedStatus": stored_status,
                        }),
                    )
                    .await?;
                    tracing::warn!(event = %raw.id, kind = ev.kind(), "anomalous event ignored");
                }
                Outcome::Stale | Outcome::Untracked => {}
            }
        }
        webhooks::save_cursor(&mut *tx, CURSOR_ID, &page.cursor, i32::try_from(ledger).unwrap_or(i32::MAX)).await?;
        tx.commit().await?;

        report.events_seen += page.events.len();
        let caught_up = cursor_ledger(&page.cursor) + 1 >= page.latest_ledger
            || saved.as_ref().and_then(|s| s.cursor.as_deref()) == Some(page.cursor.as_str());
        if page.events.len() < PAGE_LIMIT as usize && caught_up {
            report.caught_up = true;
            break;
        }
    }
    Ok(report)
}

/// Applies all stored untracked funded events for a newly installed repository.
/// Returns the number of events applied.
pub async fn apply_untracked_for_repo(db: &Db, repo_id: i64) -> Result<usize, sqlx::Error> {
    let events = untracked::get_untracked_for_repo(&db.pool, repo_id).await?;
    let mut applied = 0;
    for ev in events {
        // Check if bounty already exists (could have been created by a later reindex)
        if bounties::get(&db.pool, ev.bounty_id).await?.is_some() {
            untracked::delete_untracked_by_bounty(&db.pool, ev.bounty_id).await?;
            continue;
        }
        // Need to get the repo to have the full_name for the issue_url
        let Some(repo) = escrow_db::github::get_repo(&db.pool, repo_id).await? else {
            continue;
        };
        let issue_number = ev.issue_number;
        let intent = intents::find_unused_match(&db.pool, repo_id, issue_number, &ev.token, ev.amount_i128()).await?;
        if let Some(i) = &intent {
            intents::mark_used(&db.pool, &i.id, ev.closed_at).await?;
        }
        bounties::insert(
            &db.pool,
            &bounties::NewBounty {
                id: ev.bounty_id,
                repo_id,
                issue_number,
                issue_url: &format!("https://github.com/{}/issues/{issue_number}", repo.full_name),
                maintainer: &ev.maintainer,
                maintainer_github_id: intent.map(|i| i.requested_by),
                token: &ev.token,
                amount: ev.amount_i128(),
                fee_bps: ev.fee_bps,
                deadline: ev.deadline,
                relayer_allowed: ev.relayer_allowed,
                fund_tx: &ev.tx_hash,
                created_at: ev.closed_at,
                last_event_id: &ev.event_id,
            },
        )
        .await?;
        untracked::delete_untracked_by_bounty(&db.pool, ev.bounty_id).await?;
        applied += 1;
    }
    Ok(applied)
}

/// Returns the minimum ledger from untracked events for a repo, to tell the operator where to reindex from.
pub async fn min_untracked_ledger_for_repo(db: &Db, repo_id: i64) -> Result<Option<i32>, sqlx::Error> {
    untracked::min_untracked_ledger_for_repo(&db.pool, repo_id).await
}

pub async fn run(d: IndexerDeps, stop: CancellationToken, interval: StdDuration) {
    let mut consecutive_failures: u32 = 0;
    let mut backoff: StdDuration = StdDuration::from_secs(1);

    while !stop.is_cancelled() {
        match poll_once(&d).await {
            Ok(r) => {
                consecutive_failures = 0;
                backoff = StdDuration::from_secs(1);
                if r.applied > 0 || r.anomalies > 0 {
                    tracing::info!(?r, "indexed");
                }
                if !r.caught_up {
                    // Still behind, poll again immediately without sleeping the full interval
                    continue;
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "indexer poll failed");
                consecutive_failures += 1;
                if consecutive_failures >= FAILURE_ALERT_THRESHOLD {
                    tracing::error!(
                        error = %e,
                        consecutive_failures,
                        "indexer has failed repeatedly; alerting"
                    );
                }
                // Back off up to a cap while failing
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(StdDuration::from_secs(MAX_BACKOFF_SECS));
                continue;
            }
        }
        tokio::select! { _ = stop.cancelled() => break, _ = tokio::time::sleep(interval) => {} }
    }
}
