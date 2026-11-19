mod common;

use chrono::{Duration, Utc};
use common::*;
use escrow_chain::ChainError;
use escrow_db::{MIGRATOR, bounties, disputes, github, intents, jobs, payouts, prs, users, webhooks};
use escrow_engine::indexer::{CURSOR_ID, poll_once};
use sqlx::PgPool;

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
    payouts::release_from_pending(&pool, &p.id, "QUEUED", &win).await.unwrap();
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

    w.events.push_page(vec![resolved(5, Some(&addr(3)), 120, 1)], &event_id(120, 1), 120);
    poll_once(&w.indexer()).await.unwrap();
    let b = bounties::get(&pool, 5).await.unwrap().unwrap();
    assert_eq!((b.status.as_str(), b.winner_address.as_deref()), ("RESOLVED", Some(addr(3).as_str())));
    assert!(disputes::list_open(&pool).await.unwrap().is_empty());
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
