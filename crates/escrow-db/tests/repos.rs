//! Repository behaviour against a real Postgres. `#[sqlx::test]` creates a fresh database per test
//! (needs DATABASE_URL pointing at a server where the role may CREATE DATABASE).

use chrono::{Duration, Utc};
use escrow_db::{Db, MIGRATOR, audit, bounties, disputes, github, intents, jobs, payouts, prs, reviews, sessions, users, webhooks};
use sqlx::PgPool;

async fn seed_repo(pool: &PgPool) {
    github::upsert_installation(pool, 1, "acme", "Organization", None).await.unwrap();
    github::upsert_repo(pool, 10, 1, "acme/widgets", false).await.unwrap();
}

/// One fixed instant for the whole process, so "the same number of days" really is the same deadline.
fn base() -> chrono::DateTime<Utc> {
    static BASE: std::sync::OnceLock<chrono::DateTime<Utc>> = std::sync::OnceLock::new();
    *BASE.get_or_init(|| chrono::DateTime::from_timestamp(Utc::now().timestamp(), 0).unwrap())
}

async fn bounty(pool: &PgPool, id: i64, issue: i32, amount: i128, deadline_days: i64) {
    bounties::insert(
        pool,
        &bounties::NewBounty {
            id,
            repo_id: 10,
            issue_number: issue,
            issue_url: "https://github.com/acme/widgets/issues/1",
            maintainer: "GMAINTAINER",
            maintainer_github_id: Some(500),
            token: "CTOKEN",
            amount,
            fee_bps: 300,
            deadline: base() + Duration::days(deadline_days),
            relayer_allowed: true,
            fund_tx: "fundtx",
            created_at: Utc::now(),
            last_event_id: &format!("{id:019}-0000000001"),
        },
    )
    .await
    .unwrap();
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn schema_applies_and_pings(pool: PgPool) {
    Db::from_pool(pool).ping().await.unwrap();
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn upserting_a_github_user_refreshes_profile_and_follows_config_for_admin(pool: PgPool) {
    let a = users::upsert_github(&pool, 42, "octocat", Some("http://a"), true).await.unwrap();
    assert!(a.is_admin);
    let b = users::upsert_github(&pool, 42, "octocat-renamed", None, false).await.unwrap();
    assert_eq!(a.id, b.id, "same account, not a duplicate");
    assert_eq!(b.github_login, "octocat-renamed");
    assert!(!b.is_admin, "admin is revoked when config no longer lists the id");
    assert!(users::by_login(&pool, "OCTOCAT-RENAMED").await.unwrap().is_some(), "login lookup is case-insensitive");
    assert!(users::by_github_id(&pool, 7).await.unwrap().is_none());
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_wallet_can_belong_to_only_one_account(pool: PgPool) {
    let a = users::upsert_github(&pool, 1, "a", None, false).await.unwrap();
    let b = users::upsert_github(&pool, 2, "b", None, false).await.unwrap();
    users::set_wallet(&pool, &a.id, "GWALLET", Utc::now()).await.unwrap();
    let e = users::set_wallet(&pool, &b.id, "GWALLET", Utc::now()).await.unwrap_err();
    assert!(escrow_db::is_unique_violation(&e));
    assert_eq!(users::by_wallet(&pool, "GWALLET").await.unwrap().unwrap().id, a.id);
    users::clear_wallet(&pool, &a.id).await.unwrap();
    users::set_wallet(&pool, &b.id, "GWALLET", Utc::now()).await.unwrap();
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn sessions_expire_and_can_be_revoked(pool: PgPool) {
    let u = users::upsert_github(&pool, 1, "a", None, false).await.unwrap();
    let now = Utc::now();
    sessions::create(&pool, "hash-live", &u.id, now + Duration::days(1)).await.unwrap();
    sessions::create(&pool, "hash-dead", &u.id, now - Duration::seconds(1)).await.unwrap();
    assert!(sessions::user_for(&pool, "hash-live", now).await.unwrap().is_some());
    assert!(sessions::user_for(&pool, "hash-dead", now).await.unwrap().is_none());
    assert!(sessions::user_for(&pool, "nope", now).await.unwrap().is_none());
    assert_eq!(sessions::purge_expired(&pool, now).await.unwrap(), 1);
    sessions::delete(&pool, "hash-live").await.unwrap();
    assert!(sessions::user_for(&pool, "hash-live", now).await.unwrap().is_none());
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn of_many_simultaneous_verifications_exactly_one_burns_the_challenge(pool: PgPool) {
    let u = users::upsert_github(&pool, 1, "a", None, false).await.unwrap();
    sessions::create_challenge(&pool, "c1", &u.id, "GADDR", "msg", "nonce1", Utc::now() + Duration::minutes(5)).await.unwrap();
    let mut tasks = vec![];
    for _ in 0..12 {
        let p = pool.clone();
        tasks.push(tokio::spawn(async move { sessions::burn_challenge(&p, "c1", Utc::now()).await.unwrap() }));
    }
    let mut wins = 0;
    for t in tasks {
        wins += usize::from(t.await.unwrap());
    }
    assert_eq!(wins, 1);
    assert!(sessions::get_challenge(&pool, "c1").await.unwrap().unwrap().used_at.is_some());
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn funding_intents_match_exact_funding_and_are_single_use(pool: PgPool) {
    let exp = Utc::now() + Duration::hours(24);
    let new = |id| intents::NewIntent {
        id,
        repo_id: 10,
        issue_number: 3,
        requested_by: 500,
        token: "CTOKEN",
        amount: 50_000_000,
        duration_days: 14,
        comment_id: 9,
        expires_at: exp,
    };
    intents::create(&pool, &new("intent-aaaaaaaaaa")).await.unwrap();
    assert!(intents::find_unused_match(&pool, 10, 3, "CTOKEN", 49_999_999).await.unwrap().is_none(), "amount must match exactly");
    assert!(intents::find_unused_match(&pool, 10, 4, "CTOKEN", 50_000_000).await.unwrap().is_none(), "issue must match");
    let m = intents::find_unused_match(&pool, 10, 3, "CTOKEN", 50_000_000).await.unwrap().unwrap();
    assert_eq!((m.requested_by, m.amount_i128(), m.duration_days), (500, 50_000_000, 14));
    intents::mark_used(&pool, &m.id, Utc::now()).await.unwrap();
    assert!(intents::find_unused_match(&pool, 10, 3, "CTOKEN", 50_000_000).await.unwrap().is_none());
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn amounts_beyond_u64_round_trip_exactly(pool: PgPool) {
    seed_repo(&pool).await;
    let big = 123_456_789_012_345_678_901_234_567_890i128;
    bounty(&pool, 1, 1, big, 14).await;
    assert_eq!(bounties::get(&pool, 1).await.unwrap().unwrap().amount_i128(), big);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn status_is_only_changed_by_the_apply_functions(pool: PgPool) {
    seed_repo(&pool).await;
    bounty(&pool, 1, 1, 100, 14).await;
    assert!(bounties::find_active_for_issue(&pool, 10, 1).await.unwrap().is_some());
    bounties::apply_disputed(&pool, 1, "e2").await.unwrap();
    assert_eq!(bounties::get(&pool, 1).await.unwrap().unwrap().status, "DISPUTED");
    assert!(bounties::find_active_for_issue(&pool, 10, 1).await.unwrap().is_some(), "a disputed bounty still blocks a new one");
    bounties::apply_resolved(&pool, 1, "e3", "tx3", Some("GWIN")).await.unwrap();
    let b = bounties::get(&pool, 1).await.unwrap().unwrap();
    assert_eq!(
        (b.status.as_str(), b.close_tx.as_deref(), b.winner_address.as_deref(), b.last_event_id.as_deref()),
        ("RESOLVED", Some("tx3"), Some("GWIN"), Some("e3"))
    );
    assert!(bounties::find_active_for_issue(&pool, 10, 1).await.unwrap().is_none());
    // the CHECK constraint refuses made-up statuses
    assert!(sqlx::query("UPDATE bounties SET status = 'PAID' WHERE id = 1").execute(&pool).await.is_err());
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn listing_filters_sorts_and_paginates_without_gaps_or_repeats(pool: PgPool) {
    seed_repo(&pool).await;
    // ids 1..=7 with deliberately tied amounts and deadlines
    let spec = [(1, 500, 10), (2, 100, 3), (3, 500, 7), (4, 300, 3), (5, 100, 20), (6, 900, 7), (7, 300, 1)];
    for (id, amount, days) in spec {
        bounty(&pool, id, id as i32, amount, days).await;
    }
    for sort in [bounties::Sort::Newest, bounties::Sort::Highest, bounties::Sort::Ending] {
        let mut seen: Vec<i64> = vec![];
        let mut cursor = None;
        loop {
            let f = bounties::ListFilter { sort, cursor, limit: 3, ..Default::default() };
            let rows = bounties::list(&pool, &f).await.unwrap();
            let page: Vec<i64> = rows.iter().take(3).map(|r| r.bounty.id).collect();
            seen.extend(&page);
            if rows.len() <= 3 {
                break;
            }
            cursor = page.last().copied();
        }
        let mut uniq = seen.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(uniq.len(), 7, "{sort:?} lost or repeated rows: {seen:?}");
        match sort {
            bounties::Sort::Newest => assert_eq!(seen, [7, 6, 5, 4, 3, 2, 1]),
            bounties::Sort::Highest => assert_eq!(seen, [6, 3, 1, 7, 4, 5, 2]),
            bounties::Sort::Ending => assert_eq!(seen, [7, 4, 2, 6, 3, 1, 5]),
        }
    }
    let high = bounties::list(&pool, &bounties::ListFilter { min_amount: Some("400".into()), ..Default::default() }).await.unwrap();
    assert_eq!(high.iter().map(|r| r.bounty.id).collect::<Vec<_>>(), [6, 3, 1]);
    let none = bounties::list(&pool, &bounties::ListFilter { status: "RELEASED".into(), ..Default::default() }).await.unwrap();
    assert!(none.is_empty());
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn search_treats_wildcards_literally_and_matches_repo_or_title(pool: PgPool) {
    seed_repo(&pool).await;
    bounty(&pool, 1, 1, 100, 14).await;
    bounty(&pool, 2, 2, 100, 14).await;
    bounties::set_issue_data(&pool, 1, "100% faster parsing", &["x".into()]).await.unwrap();
    bounties::set_issue_data(&pool, 2, "Fix the thing", &[]).await.unwrap();
    async fn find(pool: &PgPool, q: &str) -> Result<Vec<bounties::Listed>, sqlx::Error> {
        bounties::list(pool, &bounties::ListFilter { q: Some(q.into()), ..Default::default() }).await
    }
    assert_eq!(find(&pool, "100%").await.unwrap().len(), 1);
    assert_eq!(find(&pool, "%").await.unwrap().len(), 1, "a lone % must not match everything");
    assert_eq!(find(&pool, "_").await.unwrap().len(), 0);
    assert_eq!(find(&pool, "WIDGETS").await.unwrap().len(), 2, "repo name, case-insensitive");
    assert_eq!(find(&pool, "thing").await.unwrap().len(), 1);
    let row = &find(&pool, "thing").await.unwrap()[0];
    assert_eq!(row.repo.full_name, "acme/widgets");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn list_reports_linked_pr_counts_and_serialises_like_the_web_app_expects(pool: PgPool) {
    seed_repo(&pool).await;
    bounty(&pool, 4, 3, 50_000_000, 14).await;
    for (id, n) in [(100, 7), (101, 8)] {
        prs::upsert(
            &pool,
            &prs::PrUpsert { id, number: n, author_github_id: 9, author_login: "dev", head_sha: "abc", state: "open", merged_at: None },
        )
        .await
        .unwrap();
        prs::link(&pool, id, 4).await.unwrap();
        prs::link(&pool, id, 4).await.unwrap(); // idempotent
    }
    let rows = bounties::list(&pool, &Default::default()).await.unwrap();
    let j = serde_json::to_value(&rows[0]).unwrap();
    assert_eq!(j["id"], "4", "ids are strings");
    assert_eq!(j["amount"], "50000000");
    assert_eq!(j["status"], "OPEN");
    assert_eq!(j["repo"]["fullName"], "acme/widgets");
    assert_eq!(j["_count"]["prs"], 2);
    assert_eq!(j["issueNumber"], 3);
    assert!(j.get("lastEventId").is_none(), "internal bookkeeping stays internal");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn only_the_first_merged_pr_creates_the_payout(pool: PgPool) {
    seed_repo(&pool).await;
    bounty(&pool, 4, 3, 100, 14).await;
    let first = payouts::create_pending(&pool, 4, 7, 900).await.unwrap().unwrap();
    assert_eq!(first.idempotency_key, "release:4");
    assert!(payouts::create_pending(&pool, 4, 8, 901).await.unwrap().is_none(), "a second PR gets no second payout");
    assert_eq!(payouts::for_bounty(&pool, 4).await.unwrap().unwrap().winner_github_id, 900);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn payout_transitions_are_guarded(pool: PgPool) {
    seed_repo(&pool).await;
    bounty(&pool, 4, 3, 100, 14).await;
    let p = payouts::create_pending(&pool, 4, 7, 900).await.unwrap().unwrap();
    let mut tasks = vec![];
    for _ in 0..8 {
        let (pool, id) = (pool.clone(), p.id.clone());
        tasks.push(tokio::spawn(async move { payouts::release_from_pending(&pool, &id, "QUEUED", "GWIN").await.unwrap() }));
    }
    let mut wins = 0;
    for t in tasks {
        wins += usize::from(t.await.unwrap());
    }
    assert_eq!(wins, 1, "exactly one caller moves it out of PENDING_WALLET");
    assert!(!payouts::release_from_pending(&pool, &p.id, "AWAITING_MAINTAINER", "GOTHER").await.unwrap());

    payouts::mark_submitted(&pool, &p.id, "hash1").await.unwrap();
    let s = payouts::get(&pool, &p.id).await.unwrap().unwrap();
    assert_eq!((s.status.as_str(), s.tx_hash.as_deref(), s.attempts), ("SUBMITTED", Some("hash1"), 1));
    assert_eq!(payouts::due(&pool, 10).await.unwrap().len(), 1);

    // the chain says it landed (maybe via another path): payout is confirmed with the chain's hash
    assert_eq!(payouts::confirm_from_chain(&pool, 4, "chainhash", "GWIN").await.unwrap(), 1);
    let c = payouts::get(&pool, &p.id).await.unwrap().unwrap();
    assert_eq!((c.status.as_str(), c.tx_hash.as_deref()), ("CONFIRMED", Some("chainhash")));
    assert_eq!(payouts::confirm_from_chain(&pool, 4, "again", "GWIN").await.unwrap(), 0, "confirming twice changes nothing");
    assert!(payouts::due(&pool, 10).await.unwrap().is_empty());
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn admin_retry_refuses_a_payout_that_has_a_transaction(pool: PgPool) {
    seed_repo(&pool).await;
    bounty(&pool, 4, 3, 100, 14).await;
    bounty(&pool, 5, 4, 100, 14).await;
    let a = payouts::create_pending(&pool, 4, 7, 900).await.unwrap().unwrap();
    let b = payouts::create_pending(&pool, 5, 7, 900).await.unwrap().unwrap();
    payouts::mark_failed(&pool, &a.id, "boom", true).await.unwrap();
    payouts::mark_submitted(&pool, &b.id, "h").await.unwrap();
    payouts::mark_failed(&pool, &b.id, "later failure", false).await.unwrap();
    assert!(payouts::retry_failed(&pool, &a.id).await.unwrap());
    assert!(!payouts::retry_failed(&pool, &b.id).await.unwrap(), "must verify the submitted tx on chain instead");
    assert_eq!(payouts::get(&pool, &a.id).await.unwrap().unwrap().status, "QUEUED");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn earnings_join_the_bounty_facts(pool: PgPool) {
    seed_repo(&pool).await;
    bounty(&pool, 4, 3, 50_000_000, 14).await;
    let p = payouts::create_pending(&pool, 4, 7, 900).await.unwrap().unwrap();
    payouts::mark_confirmed(&pool, &p.id, Some("h")).await.unwrap();
    let all = payouts::earnings(&pool, 900, false).await.unwrap();
    let j = serde_json::to_value(&all[0]).unwrap();
    assert_eq!(j["bounty"]["amount"], "50000000");
    assert_eq!(j["bounty"]["feeBps"], 300);
    assert_eq!(j["status"], "CONFIRMED");
    assert_eq!(payouts::earnings(&pool, 901, false).await.unwrap().len(), 0);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn duplicate_diffs_and_review_ranking_inputs(pool: PgPool) {
    seed_repo(&pool).await;
    bounty(&pool, 4, 3, 100, 14).await;
    for (id, n) in [(100, 7), (101, 8)] {
        prs::upsert(
            &pool,
            &prs::PrUpsert { id, number: n, author_github_id: id, author_login: "dev", head_sha: "abc", state: "open", merged_at: None },
        )
        .await
        .unwrap();
        prs::link(&pool, id, 4).await.unwrap();
    }
    prs::set_diff(&pool, 100, "samehash", "abc").await.unwrap();
    assert!(prs::has_duplicate_diff(&pool, 101, 4, "samehash").await.unwrap());
    assert!(!prs::has_duplicate_diff(&pool, 100, 4, "samehash").await.unwrap(), "a PR is not its own duplicate");

    let result = serde_json::json!({"verdict": "likely_meets"});
    let r = reviews::upsert(
        &pool,
        &reviews::NewReview {
            pr_id: 100,
            head_sha: "abc",
            model: "m",
            verdict: "likely_meets",
            score: 90,
            result: &result,
            flags: &["tests_missing".into()],
            input_tokens: 10,
            output_tokens: 5,
            comment_id: None,
        },
    )
    .await
    .unwrap();
    reviews::set_comment(&pool, &r.id, 777).await.unwrap();
    // the same (pr, sha) updates in place instead of adding a row
    reviews::upsert(
        &pool,
        &reviews::NewReview {
            pr_id: 100,
            head_sha: "abc",
            model: "m",
            verdict: "partially_meets",
            score: 60,
            result: &result,
            flags: &[],
            input_tokens: 1,
            output_tokens: 1,
            comment_id: None,
        },
    )
    .await
    .unwrap();
    let got = reviews::get(&pool, 100, "abc").await.unwrap().unwrap();
    assert_eq!((got.score, got.comment_id), (60, Some(777)), "re-review keeps the comment id");
    assert_eq!(reviews::count_for_pr_since(&pool, 100, Utc::now() - Duration::hours(1)).await.unwrap(), 1);
    assert_eq!(reviews::count_for_repo_since(&pool, 10, Utc::now() - Duration::hours(1)).await.unwrap(), 1);
    assert_eq!(reviews::count_for_repo_since(&pool, 99, Utc::now() - Duration::hours(1)).await.unwrap(), 0);

    let open = prs::open_with_latest_review(&pool, 4).await.unwrap();
    assert_eq!(open.len(), 2);
    assert_eq!(open[0].1.as_ref().map(|r| r.score), Some(60));
    assert!(open[1].1.is_none());
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn dispute_lifecycle_follows_chain_events(pool: PgPool) {
    seed_repo(&pool).await;
    bounty(&pool, 4, 3, 100, 14).await;
    let d = disputes::create(&pool, 4, 500, "no review").await.unwrap();
    assert_eq!(d.status, "requested");
    assert_eq!(disputes::count_open(&pool).await.unwrap(), 1);
    assert_eq!(disputes::mark_opened_onchain(&pool, 4).await.unwrap(), 1);
    assert_eq!(disputes::mark_opened_onchain(&pool, 4).await.unwrap(), 0);
    assert_eq!(disputes::mark_resolved(&pool, 4).await.unwrap(), 1);
    assert_eq!(disputes::count_open(&pool).await.unwrap(), 0);
    assert_eq!(disputes::count_open_older_than(&pool, Utc::now() + Duration::hours(1)).await.unwrap(), 0);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn webhook_redelivery_and_cursor_and_audit(pool: PgPool) {
    assert!(webhooks::insert_delivery(&pool, "d1", "issue_comment").await.unwrap());
    assert!(!webhooks::insert_delivery(&pool, "d1", "issue_comment").await.unwrap(), "GitHub redelivery is recognised");
    webhooks::mark_processed(&pool, "d1", Some("boom")).await.unwrap();

    assert!(webhooks::get_cursor(&pool, "bounty_escrow").await.unwrap().is_none());
    webhooks::save_cursor(&pool, "bounty_escrow", "c1", 10).await.unwrap();
    webhooks::save_cursor(&pool, "bounty_escrow", "c2", 12).await.unwrap();
    let c = webhooks::get_cursor(&pool, "bounty_escrow").await.unwrap().unwrap();
    assert_eq!((c.cursor.as_deref(), c.ledger), (Some("c2"), 12));
    webhooks::delete_cursor(&pool, "bounty_escrow").await.unwrap();
    assert!(webhooks::get_cursor(&pool, "bounty_escrow").await.unwrap().is_none());

    assert!(!audit::exists(&pool, "payout.announced", "bounty:4").await.unwrap());
    audit::log(&pool, "system", "payout.announced", "bounty:4", &serde_json::json!({})).await.unwrap();
    assert!(audit::exists(&pool, "payout.announced", "bounty:4").await.unwrap());
    assert_eq!(audit::recent(&pool, 10).await.unwrap().len(), 1);
}

// ---- the job queue ----

fn payload() -> serde_json::Value {
    serde_json::json!({"n": 1})
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn jobs_run_in_due_order_and_only_when_due(pool: PgPool) {
    let now = Utc::now();
    jobs::enqueue(&pool, "k", &serde_json::json!({"n": "later"}), now + Duration::hours(1), 5, None).await.unwrap();
    jobs::enqueue(&pool, "k", &serde_json::json!({"n": "second"}), now - Duration::seconds(1), 5, None).await.unwrap();
    jobs::enqueue(&pool, "k", &serde_json::json!({"n": "first"}), now - Duration::seconds(10), 5, None).await.unwrap();
    jobs::enqueue(&pool, "other", &payload(), now - Duration::seconds(20), 5, None).await.unwrap();
    let a = jobs::claim(&pool, &["k"], 60).await.unwrap().unwrap();
    let b = jobs::claim(&pool, &["k"], 60).await.unwrap().unwrap();
    assert_eq!((a.payload["n"].clone(), b.payload["n"].clone()), ("first".into(), "second".into()));
    assert!(jobs::claim(&pool, &["k"], 60).await.unwrap().is_none(), "the later job is not due; the other kind is not ours");
    assert_eq!(a.attempts, 1);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn concurrent_workers_never_claim_the_same_job(pool: PgPool) {
    for i in 0..30 {
        jobs::enqueue(&pool, "k", &serde_json::json!({"i": i}), Utc::now(), 5, None).await.unwrap();
    }
    let mut tasks = vec![];
    for _ in 0..6 {
        let p = pool.clone();
        tasks.push(tokio::spawn(async move {
            let mut got = vec![];
            while let Some(j) = jobs::claim(&p, &["k"], 60).await.unwrap() {
                got.push(j.id);
            }
            got
        }));
    }
    let mut all = vec![];
    for t in tasks {
        all.extend(t.await.unwrap());
    }
    all.sort_unstable();
    let n = all.len();
    all.dedup();
    assert_eq!((n, all.len()), (30, 30), "every job claimed exactly once");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn failures_back_off_then_die_and_success_completes(pool: PgPool) {
    jobs::enqueue(&pool, "k", &payload(), Utc::now(), 2, None).await.unwrap();
    let j = jobs::claim(&pool, &["k"], 60).await.unwrap().unwrap();
    assert!(!jobs::fail(&pool, j.id, "try 1", 3600).await.unwrap());
    assert!(jobs::claim(&pool, &["k"], 60).await.unwrap().is_none(), "backed off for an hour");
    sqlx::query("UPDATE jobs SET run_at = now() WHERE id = $1").bind(j.id).execute(&pool).await.unwrap();
    let j2 = jobs::claim(&pool, &["k"], 60).await.unwrap().unwrap();
    assert_eq!(j2.attempts, 2);
    assert!(jobs::fail(&pool, j2.id, "try 2", 1).await.unwrap(), "out of attempts: dead");
    assert_eq!(jobs::count_dead(&pool).await.unwrap(), 1);
    assert!(jobs::claim(&pool, &["k"], 60).await.unwrap().is_none());

    jobs::enqueue(&pool, "k", &payload(), Utc::now(), 2, None).await.unwrap();
    let ok = jobs::claim(&pool, &["k"], 60).await.unwrap().unwrap();
    jobs::complete(&pool, ok.id).await.unwrap();
    assert_eq!(jobs::count_pending(&pool).await.unwrap(), 0);
    assert_eq!(jobs::purge_done(&pool, Utc::now() + Duration::hours(1)).await.unwrap(), 1);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_crashed_workers_job_is_reclaimed_after_its_lease(pool: PgPool) {
    jobs::enqueue(&pool, "k", &payload(), Utc::now(), 3, None).await.unwrap();
    let j = jobs::claim(&pool, &["k"], 3600).await.unwrap().unwrap();
    assert!(jobs::claim(&pool, &["k"], 60).await.unwrap().is_none(), "lease still held");
    sqlx::query("UPDATE jobs SET locked_until = now() - interval '1 second' WHERE id = $1").bind(j.id).execute(&pool).await.unwrap();
    let again = jobs::claim(&pool, &["k"], 60).await.unwrap().unwrap();
    assert_eq!((again.id, again.attempts), (j.id, 2));

    // a lease that expires with no attempts left is reaped, not retried forever
    sqlx::query("UPDATE jobs SET locked_until = now() - interval '1 second', attempts = max_attempts WHERE id = $1")
        .bind(j.id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(jobs::claim(&pool, &["k"], 60).await.unwrap().is_none());
    assert_eq!(jobs::reap_exhausted(&pool).await.unwrap(), 1);
    assert_eq!(jobs::count_dead(&pool).await.unwrap(), 1);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn dedupe_keys_collapse_queued_jobs_and_debounce_moves_the_run_time(pool: PgPool) {
    let now = Utc::now();
    assert!(jobs::enqueue(&pool, "k", &payload(), now, 5, Some("same")).await.unwrap());
    assert!(!jobs::enqueue(&pool, "k", &payload(), now, 5, Some("same")).await.unwrap(), "already queued");

    // debounce: a newer push pushes the review out instead of adding another
    jobs::enqueue_debounced(&pool, "review", &serde_json::json!({"v": 1}), now + Duration::minutes(5), 3, "review:7").await.unwrap();
    jobs::enqueue_debounced(&pool, "review", &serde_json::json!({"v": 2}), now + Duration::minutes(10), 3, "review:7").await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE dedupe_key = 'review:7'").fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1);
    let (v, run_at): (serde_json::Value, chrono::DateTime<Utc>) =
        sqlx::query_as("SELECT payload, run_at FROM jobs WHERE dedupe_key = 'review:7'").fetch_one(&pool).await.unwrap();
    assert_eq!(v["v"], 2);
    assert!(run_at > now + Duration::minutes(9));

    // once the first is running, a new request queues behind it
    sqlx::query("UPDATE jobs SET run_at = now() WHERE dedupe_key = 'review:7'").execute(&pool).await.unwrap();
    let running = jobs::claim(&pool, &["review"], 60).await.unwrap().unwrap();
    jobs::enqueue_debounced(&pool, "review", &serde_json::json!({"v": 3}), now, 3, "review:7").await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE dedupe_key = 'review:7'").fetch_one(&pool).await.unwrap();
    assert_eq!(n, 2, "the running job is untouched; the new one waits");
    let _ = running;
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn the_card_lock_serialises_concurrent_refreshes(pool: PgPool) {
    seed_repo(&pool).await;
    bounty(&pool, 4, 3, 100, 14).await;
    let posted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut tasks = vec![];
    for _ in 0..6 {
        let (pool, posted) = (pool.clone(), posted.clone());
        tasks.push(tokio::spawn(async move {
            let mut tx = pool.begin().await.unwrap();
            bounties::lock_card(&mut tx, 4).await.unwrap();
            if bounties::card_comment(&mut *tx, 4).await.unwrap().is_none() {
                tokio::time::sleep(std::time::Duration::from_millis(40)).await; // the GitHub call
                posted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                bounties::set_card_comment(&mut *tx, 4, 999).await.unwrap();
            }
            tx.commit().await.unwrap();
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    assert_eq!(posted.load(std::sync::atomic::Ordering::SeqCst), 1, "exactly one card is posted");
}
