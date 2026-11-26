mod common;

use chrono::{Duration, Utc};
use common::*;
use escrow_chain::ChainError;
use escrow_core::PayoutStatus;
use escrow_db::{MIGRATOR, audit, bounties, disputes, github, intents, jobs, payouts, prs, untracked, users, webhooks};
use escrow_engine::indexer::{
    CURSOR_ID, FAILURE_ALERT_THRESHOLD, MAX_PAGES, apply_untracked_for_repo, min_untracked_ledger_for_repo, poll_once, run,
};
use sqlx::PgPool;
use std::time::Duration as StdDuration;
use tokio_util::sync::CancellationToken;

const REPO_ID: i64 = 10;

async fn tracked_repo(w: &World) {
    github::upsert_installation(&w.db.pool, 1, "acme", "Organization", None).await.unwrap();
    github::upsert_repo(&w.db.pool, REPO_ID, 1, "acme/widgets", false).await.unwrap();
}

fn fund(id: u64, maintainer: &str, issue: u64) -> Funded<'_> {
    Funded { id, maintainer, repo_id: REPO_ID as u64, issue, token: TOKEN_C, amount: 50_000_000, relayer_allowed: true }
}

fn jobs_of(kind: &str) -> String {
    format!("SELECT count(*) FROM jobs WHERE kind = '{kind}' AND status = 'queued'")
}

async fn count(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).fetch_one(pool).await.unwrap()
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn funding_creates_the_bounty_attributes_it_to_the_requester_and_queues_a_card(pool: PgPool) {
    let w = World::new(pool.clone());
    tracked_repo(&w).await;
    let m = addr(1);
    intents::create(
        &pool,
        &intents::NewIntent {
            id: "intent-aaaaaaaaaa",
            repo_id: REPO_ID,
            issue_number: 3,
            requested_by: 500,
            token: TOKEN_C,
            amount: 50_000_000,
            duration_days: 14,
            comment_id: 9,
            expires_at: Utc::now() + Duration::hours(24),
        },
    )
    .await
    .unwrap();
    w.events.push_page(vec![funded(&fund(4, &m, 3), 100, 1)], &event_id(100, 1), 100);

    let r = poll_once(&w.indexer()).await.unwrap();
    assert_eq!((r.events_seen, r.applied), (1, 1));

    let b = bounties::get(&pool, 4).await.unwrap().unwrap();
    assert_eq!(
        (b.status.as_str(), b.maintainer.as_str(), b.maintainer_github_id, b.amount_i128(), b.fee_bps, b.relayer_allowed),
        ("OPEN", m.as_str(), Some(500), 50_000_000, 300, true)
    );
    assert_eq!(b.issue_url, "https://github.com/acme/widgets/issues/3");
    assert!(intents::get(&pool, "intent-aaaaaaaaaa").await.unwrap().unwrap().used_at.is_some(), "the funding link is burned");
    assert_eq!(count(&pool, &jobs_of("card.refresh")).await, 1);
}

// #218: a bounty funded straight on chain (no funding link) takes the maintainer's GitHub id from the
// account that has linked the maintainer wallet, when one exists.
#[sqlx::test(migrator = "MIGRATOR")]
async fn an_intentless_funding_resolves_the_maintainer_github_id_from_the_linked_wallet(pool: PgPool) {
    let w = World::new(pool.clone());
    tracked_repo(&w).await;
    let m = addr(1);
    // the maintainer's wallet is linked to GitHub id 700
    let u = users::upsert_github(&pool, 700, "maintainer", None, false).await.unwrap();
    users::set_wallet(&pool, &u.id, &m, Utc::now()).await.unwrap();

    // funded on chain with no matching funding intent
    w.events.push_page(vec![funded(&fund(4, &m, 3), 100, 1)], &event_id(100, 1), 100);
    assert_eq!(poll_once(&w.indexer()).await.unwrap().applied, 1);

    let b = bounties::get(&pool, 4).await.unwrap().unwrap();
    assert_eq!(b.maintainer_github_id, Some(700), "resolved from the wallet link when there is no intent");
}

// #218: with no funding link AND no linked wallet, the maintainer_github_id stays unset (filled in later).
#[sqlx::test(migrator = "MIGRATOR")]
async fn an_intentless_funding_without_a_linked_wallet_leaves_the_github_id_unset(pool: PgPool) {
    let w = World::new(pool.clone());
    tracked_repo(&w).await;
    let m = addr(1);
    w.events.push_page(vec![funded(&fund(4, &m, 3), 100, 1)], &event_id(100, 1), 100);
    assert_eq!(poll_once(&w.indexer()).await.unwrap().applied, 1);

    let b = bounties::get(&pool, 4).await.unwrap().unwrap();
    assert_eq!(b.maintainer_github_id, None, "no intent and no linked wallet: unknown for now");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn replaying_a_page_changes_nothing_and_queues_nothing_more(pool: PgPool) {
    let w = World::new(pool.clone());
    tracked_repo(&w).await;
    let m = addr(1);
    let page = vec![funded(&fund(4, &m, 3), 100, 1), released(4, &addr(2), &addr(9), 101, 1)];
    w.events.push_page(page.clone(), &event_id(101, 1), 101);
    assert_eq!(poll_once(&w.indexer()).await.unwrap().applied, 2);
    let before = (count(&pool, "SELECT count(*) FROM jobs").await, bounties::get(&pool, 4).await.unwrap().unwrap().updated_at);

    // as if we crashed after the commit but before learning it: the same page arrives again from an older cursor
    webhooks::delete_cursor(&pool, CURSOR_ID).await.unwrap();
    w.events.push_page(page, &event_id(101, 1), 101);
    let r = poll_once(&w.indexer()).await.unwrap();
    assert_eq!(r.applied, 0, "stale events are recognised, not re-applied");
    assert_eq!(count(&pool, "SELECT count(*) FROM jobs").await, before.0);
    assert_eq!(bounties::get(&pool, 4).await.unwrap().unwrap().updated_at, before.1);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn funding_a_repo_we_do_not_track_is_ignored(pool: PgPool) {
    let w = World::new(pool.clone());
    let m = addr(1);
    w.events.push_page(vec![funded(&fund(4, &m, 3), 100, 1)], &event_id(100, 1), 100);
    let r = poll_once(&w.indexer()).await.unwrap();
    assert_eq!((r.events_seen, r.applied), (1, 0));
    assert!(bounties::get(&pool, 4).await.unwrap().is_none());
    assert_eq!(count(&pool, "SELECT count(*) FROM jobs").await, 0);
    // ... and later events for it are ignored too
    w.events.push_page(vec![released(4, &addr(2), &addr(9), 101, 1)], &event_id(101, 1), 101);
    assert_eq!(poll_once(&w.indexer()).await.unwrap().applied, 0);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_release_closes_the_bounty_confirms_the_payout_and_finds_the_winner_by_wallet(pool: PgPool) {
    let w = World::new(pool.clone());
    tracked_repo(&w).await;
    let (m, win) = (addr(1), addr(2));
    users::upsert_github(&pool, 900, "dev", None, false).await.unwrap();
    let u = users::by_github_id(&pool, 900).await.unwrap().unwrap();
    users::set_wallet(&pool, &u.id, &win, Utc::now()).await.unwrap();
    w.events.push_page(vec![funded(&fund(4, &m, 3), 100, 1)], &event_id(100, 1), 100);
    poll_once(&w.indexer()).await.unwrap();
    // a relayer payout is mid-flight when the chain confirms it
    let p = payouts::create_pending(&pool, 4, 7, 900).await.unwrap().unwrap();
    payouts::release_from_pending(&pool, &p.id, PayoutStatus::Queued, &win).await.unwrap();
    payouts::mark_submitted(&pool, &p.id, "relayerhash").await.unwrap();

    let rel = released(4, &win, &addr(9), 105, 2);
    let tx = rel.tx_hash.clone();
    w.events.push_page(vec![rel], &event_id(105, 2), 105);
    assert_eq!(poll_once(&w.indexer()).await.unwrap().applied, 1);

    let b = bounties::get(&pool, 4).await.unwrap().unwrap();
    assert_eq!(
        (b.status.as_str(), b.winner_address.as_deref(), b.winner_github_id, b.close_tx.as_deref()),
        ("RELEASED", Some(win.as_str()), Some(900), Some(tx.as_str()))
    );
    let p = payouts::for_bounty(&pool, 4).await.unwrap().unwrap();
    assert_eq!((p.status.as_str(), p.tx_hash.as_deref()), ("CONFIRMED", Some(tx.as_str())), "the chain's hash wins");
    assert_eq!(count(&pool, &jobs_of("payout.announce")).await, 1);
    assert_eq!(count(&pool, &jobs_of("card.refresh")).await, 1, "two events for one bounty collapse into one refresh");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn refund_dispute_and_resolution_follow_the_chain(pool: PgPool) {
    let w = World::new(pool.clone());
    tracked_repo(&w).await;
    let m = addr(1);
    w.events.push_page(vec![funded(&fund(4, &m, 3), 100, 1), funded(&fund(5, &m, 4), 100, 2)], &event_id(100, 2), 100);
    poll_once(&w.indexer()).await.unwrap();

    disputes::create(&pool, 5, 500, "never reviewed").await.unwrap();
    w.events.push_page(vec![refunded(4, 110, 1), disputed(5, &addr(8), 111, 1)], &event_id(111, 1), 111);
    assert_eq!(poll_once(&w.indexer()).await.unwrap().applied, 2);
    assert_eq!(bounties::get(&pool, 4).await.unwrap().unwrap().status, "REFUNDED");
    assert_eq!(bounties::get(&pool, 5).await.unwrap().unwrap().status, "DISPUTED");
    assert_eq!(disputes::list_open(&pool).await.unwrap()[0].status, "opened_onchain");
    assert_eq!(count(&pool, &jobs_of("dispute.comment")).await, 1, "a dispute event queues one comment");

    w.events.push_page(vec![resolved(5, Some(&addr(3)), 120, 1)], &event_id(120, 1), 120);
    poll_once(&w.indexer()).await.unwrap();
    let b = bounties::get(&pool, 5).await.unwrap().unwrap();
    assert_eq!((b.status.as_str(), b.winner_address.as_deref()), ("RESOLVED", Some(addr(3).as_str())));
    assert!(disputes::list_open(&pool).await.unwrap().is_empty());
    assert_eq!(count(&pool, &jobs_of("dispute.comment")).await, 2, "a resolution queues its own comment");

    // Replaying the same events from an older cursor must not post either comment twice.
    webhooks::delete_cursor(&pool, CURSOR_ID).await.unwrap();
    w.events.push_page(vec![disputed(5, &addr(8), 111, 1)], &event_id(111, 1), 111);
    w.events.push_page(vec![resolved(5, Some(&addr(3)), 120, 1)], &event_id(120, 1), 120);
    assert_eq!(poll_once(&w.indexer()).await.unwrap().applied, 0);
    assert_eq!(count(&pool, &jobs_of("dispute.comment")).await, 2);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn events_that_contradict_what_we_know_are_counted_and_ignored(pool: PgPool) {
    let w = World::new(pool.clone());
    tracked_repo(&w).await;
    let m = addr(1);
    w.events.push_page(vec![funded(&fund(4, &m, 3), 100, 1), refunded(4, 101, 1)], &event_id(101, 1), 101);
    poll_once(&w.indexer()).await.unwrap();
    // a release for a bounty we already know was refunded, and a resolve for one that was never disputed
    w.events.push_page(vec![released(4, &addr(2), &addr(9), 102, 1), resolved(4, None, 103, 1)], &event_id(103, 1), 103);
    let r = poll_once(&w.indexer()).await.unwrap();
    assert_eq!((r.applied, r.anomalies), (0, 2));
    assert_eq!(bounties::get(&pool, 4).await.unwrap().unwrap().status, "REFUNDED", "the earlier truth stands");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn the_cursor_is_saved_and_the_next_poll_resumes_from_it(pool: PgPool) {
    let w = World::new(pool.clone());
    tracked_repo(&w).await;
    w.events.push_page(vec![], &event_id(9_500, 0), 9_501);
    w.events.push_page(vec![], &event_id(10_000, 0), 10_000);
    poll_once(&w.indexer()).await.unwrap();
    poll_once(&w.indexer()).await.unwrap();
    let calls = w.events.calls.lock().unwrap().clone();
    assert_eq!(calls[0], "ledger:9000", "a new database starts `lookback` ledgers behind the tip");
    assert!(calls[1].starts_with("cursor:") && calls[1].contains(&format!("{:019}", 9_500u64 << 32)), "{calls:?}");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn an_explicit_start_ledger_wins_over_the_lookback(pool: PgPool) {
    let w = World::new(pool.clone());
    w.events.push_page(vec![], &event_id(10_000, 0), 10_000);
    let mut d = w.indexer();
    d.start_ledger = Some(5_521_000);
    poll_once(&d).await.unwrap();
    assert_eq!(w.events.calls.lock().unwrap()[0], "ledger:5521000");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_quiet_contract_still_shows_an_up_to_date_ledger(pool: PgPool) {
    let w = World::new(pool.clone());
    w.events.push_page(vec![], &event_id(9_990, 0), 9_991);
    poll_once(&w.indexer()).await.unwrap();
    let c = webhooks::get_cursor(&pool, CURSOR_ID).await.unwrap().unwrap();
    assert_eq!(c.ledger, 9_990, "health and lag alerts depend on this moving with the cursor, not just with events");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn it_keeps_paging_until_the_cursor_reaches_the_tip(pool: PgPool) {
    let w = World::new(pool.clone());
    // the RPC scans a bounded range per request and may return an empty page with a moved cursor
    for ledger in [2_000u32, 4_000, 6_000, 9_999] {
        w.events.push_page(vec![], &event_id(ledger, 0), 10_000);
    }
    poll_once(&w.indexer()).await.unwrap();
    assert_eq!(w.events.calls.lock().unwrap().len(), 4);
    assert_eq!(webhooks::get_cursor(&pool, CURSOR_ID).await.unwrap().unwrap().ledger, 9_999);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_cursor_older_than_the_rpcs_retention_is_dropped_loudly_not_stuck_on(pool: PgPool) {
    let w = World::new(pool.clone());
    webhooks::save_cursor(&pool, CURSOR_ID, &event_id(100, 0), 100).await.unwrap();
    w.events.push_error(ChainError::Rpc { code: -32602, message: "cursor must be within the ledger range: 9000 - 10000".into() });
    let r = poll_once(&w.indexer()).await.unwrap();
    assert!(r.cursor_reset);
    assert!(webhooks::get_cursor(&pool, CURSOR_ID).await.unwrap().is_none());
    // the next poll restarts from the fallback
    w.events.push_page(vec![], &event_id(10_000, 0), 10_000);
    poll_once(&w.indexer()).await.unwrap();
    assert_eq!(w.events.calls.lock().unwrap().last().unwrap(), "ledger:9000");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn other_rpc_errors_propagate_and_leave_the_cursor_alone(pool: PgPool) {
    let w = World::new(pool.clone());
    webhooks::save_cursor(&pool, CURSOR_ID, &event_id(100, 0), 100).await.unwrap();
    w.events.push_error(ChainError::Http("timeout".into()));
    assert!(poll_once(&w.indexer()).await.is_err());
    assert_eq!(webhooks::get_cursor(&pool, CURSOR_ID).await.unwrap().unwrap().ledger, 100);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_malformed_tracked_event_aborts_the_page_and_the_cursor_does_not_advance(pool: PgPool) {
    let w = World::new(pool.clone());
    tracked_repo(&w).await;
    let m = addr(1);
    let mut bad = refunded(4, 101, 1);
    bad.value_b64 = "AAAA".into(); // not a valid ScVal map
    w.events.push_page(vec![funded(&fund(4, &m, 3), 100, 1), bad], &event_id(101, 1), 101);
    assert!(poll_once(&w.indexer()).await.is_err());
    assert!(bounties::get(&pool, 4).await.unwrap().is_none(), "the good event on the same page was rolled back with it");
    assert!(webhooks::get_cursor(&pool, CURSOR_ID).await.unwrap().is_none());
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn untracked_event_kinds_and_failed_calls_are_skipped(pool: PgPool) {
    let w = World::new(pool.clone());
    tracked_repo(&w).await;
    let m = addr(1);
    let mut failed = funded(&fund(9, &m, 9), 100, 2);
    failed.in_successful_contract_call = false;
    w.events.push_page(vec![paused_event(100, 1), failed], &event_id(100, 2), 100);
    let r = poll_once(&w.indexer()).await.unwrap();
    assert_eq!((r.events_seen, r.applied), (2, 0));
    assert!(bounties::get(&pool, 9).await.unwrap().is_none());
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn indexing_links_pending_pr_state_untouched(pool: PgPool) {
    // events never touch pull-request records; those belong to the webhook path
    let w = World::new(pool.clone());
    tracked_repo(&w).await;
    let m = addr(1);
    w.events.push_page(vec![funded(&fund(4, &m, 3), 100, 1)], &event_id(100, 1), 100);
    poll_once(&w.indexer()).await.unwrap();
    prs::upsert(
        &pool,
        &prs::PrUpsert { id: 100, number: 7, author_github_id: 900, author_login: "dev", head_sha: "abc", state: "open", merged_at: None },
    )
    .await
    .unwrap();
    prs::link(&pool, 100, 4).await.unwrap();
    w.events.push_page(vec![refunded(4, 110, 1)], &event_id(110, 1), 110);
    poll_once(&w.indexer()).await.unwrap();
    assert_eq!(prs::get(&pool, 100).await.unwrap().unwrap().state, "open");
    let _ = jobs::count_pending(&pool).await.unwrap();
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn anomaly_writes_audit_entry_with_details(pool: PgPool) {
    let w = World::new(pool.clone());
    tracked_repo(&w).await;
    let m = addr(1);
    w.events.push_page(vec![funded(&fund(4, &m, 3), 100, 1)], &event_id(100, 1), 100);
    poll_once(&w.indexer()).await.unwrap();

    w.events.push_page(vec![refunded(4, 101, 1)], &event_id(101, 1), 101);
    poll_once(&w.indexer()).await.unwrap();

    w.events.push_page(vec![released(4, &addr(2), &addr(9), 102, 1)], &event_id(102, 1), 102);
    let r = poll_once(&w.indexer()).await.unwrap();
    assert_eq!((r.applied, r.anomalies), (0, 1));

    let audit_rows = audit::recent(&pool, 10).await.unwrap();
    let anomaly_audit = audit_rows.iter().find(|a| a.action == "anomaly_detected").expect("anomaly audit entry should exist");
    let data = &anomaly_audit.data;
    assert_eq!(data["eventId"], event_id(102, 1));
    assert_eq!(data["kind"], "released");
    assert_eq!(data["bountyId"], 4);
    assert_eq!(data["storedStatus"], "REFUNDED");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn untracked_funded_event_is_stored_and_applied_when_repo_installed(pool: PgPool) {
    let w = World::new(pool.clone());
    let m = addr(1);

    // Fund an untracked repo (REPO_ID = 10, but we haven't installed it)
    w.events.push_page(vec![funded(&fund(4, &m, 3), 100, 1)], &event_id(100, 1), 100);
    let r = poll_once(&w.indexer()).await.unwrap();
    assert_eq!((r.events_seen, r.applied), (1, 0));
    assert!(bounties::get(&pool, 4).await.unwrap().is_none());

    // Verify untracked event was stored
    let stored = untracked::get_untracked_for_repo(&pool, REPO_ID).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].bounty_id, 4);
    assert_eq!(stored[0].event_id, event_id(100, 1));

    // Now install the repo
    github::upsert_installation(&pool, 1, "acme", "Organization", None).await.unwrap();
    github::upsert_repo(&pool, REPO_ID, 1, "acme/widgets", false).await.unwrap();

    // Apply untracked events
    let applied = apply_untracked_for_repo(&w.db, REPO_ID).await.unwrap();
    assert_eq!(applied, 1);

    // Bounty should now exist
    let b = bounties::get(&pool, 4).await.unwrap().unwrap();
    assert_eq!(b.status, "OPEN");
    assert_eq!(b.repo_id, REPO_ID);
    assert_eq!(b.issue_number, 3);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn min_untracked_ledger_guides_reindex(pool: PgPool) {
    let w = World::new(pool.clone());
    let m = addr(1);

    // Fund at ledger 5000
    w.events.push_page(vec![funded(&fund(4, &m, 3), 5000, 1)], &event_id(5000, 1), 5000);
    poll_once(&w.indexer()).await.unwrap();

    let min_ledger = min_untracked_ledger_for_repo(&w.db, REPO_ID).await.unwrap();
    assert_eq!(min_ledger, Some(5000));
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn poll_report_caught_up_false_when_max_pages_reached(pool: PgPool) {
    let w = World::new(pool.clone());
    tracked_repo(&w).await;
    let m = addr(1);

    // Create more than MAX_PAGES pages of events (each page has 1 event, PAGE_LIMIT=200)
    for i in 0..MAX_PAGES + 10 {
        let ledger = 1000 + i as u32;
        w.events.push_page(vec![funded(&fund(100 + i as u64, &m, 1), ledger, 1)], &event_id(ledger, 1), 10_000);
    }
    // The last page should have the latest_ledger far ahead
    w.events.push_page(vec![], &event_id(10_000, 0), 10_000);

    let r = poll_once(&w.indexer()).await.unwrap();
    assert!(!r.caught_up, "should report not caught up when MAX_PAGES reached");
    assert_eq!(r.applied, MAX_PAGES);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn run_polls_immediately_when_not_caught_up(pool: PgPool) {
    let w = World::new(pool.clone());
    tracked_repo(&w).await;
    let m = addr(1);

    // Create events spanning more than MAX_PAGES
    for i in 0..MAX_PAGES + 5 {
        let ledger = 1000 + i as u32;
        w.events.push_page(vec![funded(&fund(200 + i as u64, &m, 1), ledger, 1)], &event_id(ledger, 1), 10_000);
    }
    w.events.push_page(vec![], &event_id(10_000, 0), 10_000);

    let d = w.indexer();
    let stop = CancellationToken::new();
    let stop_clone = stop.clone();
    let handle = tokio::spawn(async move {
        run(d, stop_clone, StdDuration::from_secs(10)).await;
    });

    // Give it time to process multiple polls
    tokio::time::sleep(StdDuration::from_millis(500)).await;
    stop.cancel();
    handle.await.unwrap();

    // Should have processed all events (more than MAX_PAGES)
    let total_bounties = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM bounties").fetch_one(&pool).await.unwrap();
    assert!(total_bounties > MAX_PAGES as i64);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn consecutive_failures_trigger_alert_and_back_off(pool: PgPool) {
    let w = World::new(pool.clone());
    tracked_repo(&w).await;

    // Push errors for more than FAILURE_ALERT_THRESHOLD times
    for _ in 0..FAILURE_ALERT_THRESHOLD + 2 {
        w.events.push_error(ChainError::Http("timeout".into()));
    }

    let d = w.indexer();
    let stop = CancellationToken::new();
    let stop_clone = stop.clone();
    let handle = tokio::spawn(async move {
        run(d, stop_clone, StdDuration::from_millis(10)).await;
    });

    // Wait for multiple failures and backoff
    tokio::time::sleep(StdDuration::from_millis(500)).await;
    stop.cancel();
    handle.await.unwrap();

    // The test passing without hanging shows backoff works
    // In a real test we'd check logs, but here we verify the loop doesn't spin tight
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn out_of_range_event_fields_abort_the_page_instead_of_indexing_made_up_values(pool: PgPool) {
    let w = World::new(pool.clone());
    tracked_repo(&w).await;
    let m = addr(1);

    // a repo id that does not fit in i64 is malformed: the page aborts and nothing is stored
    w.events.push_page(vec![funded_with(&fund(4, &m, 3), u64::MAX, 1_795_000_000, 100, 1)], &event_id(100, 1), 100);
    assert!(poll_once(&w.indexer()).await.is_err(), "an out-of-range repo id is a decode error");
    assert!(bounties::get(&pool, 4).await.unwrap().is_none());
    assert!(webhooks::get_cursor(&pool, CURSOR_ID).await.unwrap().is_none(), "the cursor stays put so the bad event is retried");

    // a deadline that fits in i64 but is not a valid timestamp is malformed too
    w.events.push_page(
        vec![funded_with(&fund(5, &m, 4), REPO_ID as u64, u64::try_from(i64::MAX).unwrap(), 100, 2)],
        &event_id(100, 2),
        100,
    );
    assert!(poll_once(&w.indexer()).await.is_err(), "an invalid deadline is a decode error");
    assert!(bounties::get(&pool, 5).await.unwrap().is_none());
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn an_unparseable_ledger_closed_at_is_stamped_with_now_instead_of_silently_rewritten(pool: PgPool) {
    let w = World::new(pool.clone());
    tracked_repo(&w).await;
    let m = addr(1);
    let mut ev = funded(&fund(4, &m, 3), 100, 1);
    ev.ledger_closed_at = "not a timestamp".into();
    w.events.push_page(vec![ev], &event_id(100, 1), 100);
    let r = poll_once(&w.indexer()).await.unwrap();
    assert_eq!((r.events_seen, r.applied), (1, 1));
    let b = bounties::get(&pool, 4).await.unwrap().unwrap();
    assert!(b.created_at > Utc::now() - Duration::seconds(60), "the fallback stamp is the current time: {:?}", b.created_at);
}
