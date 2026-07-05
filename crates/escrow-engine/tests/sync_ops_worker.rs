mod common;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use common::*;
use escrow_chain::{ChainBounty, ChainError, ChainReader, ContractConfig};
use escrow_core::BountyStatus;
use escrow_db::{MIGRATOR, audit, bounties, disputes, jobs, payouts, prs, webhooks};
use escrow_engine::{
    indexer::CURSOR_ID,
    jobs as kinds,
    ops::{AlertInputs, AlertSender, collect_alerts, health_report},
    ports::{ReviewFailure, ReviewRunner},
    sync::{
        CardOutcome, GithubCommenter, GithubNotifier, IssueCommenter, announce_payout, refresh_bounty_card, send_refund_reminders,
        sync_installations,
    },
    worker::{WorkerDeps, handle_job, run_queue},
};
use escrow_github::{GithubError, InstallationInfo, RepoInfo};
use escrow_payouts::Notifier;
use escrow_testkit::seed;
use serde_json::json;
use sqlx::PgPool;
use std::{
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

async fn world(pool: PgPool) -> World {
    let w = World::new(pool);
    seed::repo(&w.db).await;
    w.gh.set_issue(seed::REPO, 3, "Retry failed HTTP requests", "## Acceptance criteria\n- Retries up to 3 times\n- Adds a test");
    w
}

async fn open_bounty(w: &World, id: i64, issue: i32) {
    let mut b = seed::B::new(id, issue);
    b.deadline = chrono::DateTime::from_timestamp(Utc::now().timestamp() + 14 * 86_400, 0).unwrap();
    seed::bounty(&w.db, &b).await;
    sqlx::query("UPDATE bounties SET token = $2 WHERE id = $1").bind(id).bind(TOKEN_XLM).execute(&w.db.pool).await.unwrap();
}

// ---------------- the bounty card ----------------

#[sqlx::test(migrator = "MIGRATOR")]
async fn the_card_is_posted_once_then_edited_in_place_and_the_label_is_set(pool: PgPool) {
    let w = world(pool.clone()).await;
    open_bounty(&w, 4, 3).await;
    assert_eq!(refresh_bounty_card(&w.sync(), 4).await.unwrap(), CardOutcome::Posted);
    let card = w.gh.comments()[0].clone();
    assert_eq!(card.issue, 3);
    assert!(
        card.body.contains("### SkillEscrow bounty: 5.00 XLM · 🟢 Open")
            && card.body.contains("- Retries up to 3 times")
            && card.body.contains("`Closes #3`"),
        "{}",
        card.body
    );
    assert_eq!(w.gh.with(|s| s.labels.clone()), [(seed::REPO.to_string(), 3, "bounty: 5 XLM".to_string())]);
    let b = bounties::get(&pool, 4).await.unwrap().unwrap();
    assert_eq!((b.issue_title.as_str(), b.criteria.len(), b.card_comment_id), ("Retry failed HTTP requests", 2, Some(card.id)));

    assert_eq!(refresh_bounty_card(&w.sync(), 4).await.unwrap(), CardOutcome::Updated);
    assert_eq!(w.gh.comments().len(), 1, "still one card");
    assert_eq!(w.gh.with(|s| s.edits.len()), 1);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn the_card_reflects_the_new_status_and_drops_the_label_when_the_bounty_closes(pool: PgPool) {
    let w = world(pool.clone()).await;
    open_bounty(&w, 4, 3).await;
    refresh_bounty_card(&w.sync(), 4).await.unwrap();
    bounties::apply_released(&pool, 4, "e9", "txhash", "GWIN", None).await.unwrap();
    refresh_bounty_card(&w.sync(), 4).await.unwrap();
    let edited = w.gh.with(|s| s.edits.last().cloned()).unwrap().1;
    assert!(edited.contains("✅ Paid") && !edited.contains("How to claim"));
    assert_eq!(w.gh.with(|s| s.labels.len()), 1, "no new label for a closed bounty");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn concurrent_refreshes_post_exactly_one_card(pool: PgPool) {
    let w = world(pool.clone()).await;
    open_bounty(&w, 4, 3).await;
    w.gh.with(|s| s.delay_ms = 30); // the GitHub round-trip that used to let two refreshes both see "no card yet"
    let mut tasks = vec![];
    for _ in 0..6 {
        let d = w.sync();
        tasks.push(tokio::spawn(async move { refresh_bounty_card(&d, 4).await.unwrap() }));
    }
    let mut posted = 0;
    for t in tasks {
        posted += usize::from(t.await.unwrap() == CardOutcome::Posted);
    }
    assert_eq!(posted, 1);
    assert_eq!(w.gh.comments().len(), 1);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn if_the_issue_cannot_be_read_the_card_renders_from_stored_data(pool: PgPool) {
    let w = world(pool.clone()).await;
    open_bounty(&w, 4, 3).await;
    bounties::set_issue_data(&pool, 4, "Stored title", &["stored criterion".into()]).await.unwrap();
    w.gh.with(|s| s.issues.clear());
    assert_eq!(refresh_bounty_card(&w.sync(), 4).await.unwrap(), CardOutcome::Posted);
    assert!(w.gh.comments()[0].body.contains("stored criterion"));
    assert_eq!(refresh_bounty_card(&w.sync(), 999).await.unwrap(), CardOutcome::Skipped);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn hostile_criteria_text_cannot_inject_markup_into_the_card(pool: PgPool) {
    let w = world(pool.clone()).await;
    open_bounty(&w, 4, 3).await;
    w.gh.set_issue(seed::REPO, 3, "t", "## Acceptance criteria\n- [click me](http://evil.example) @everyone <img src=x>");
    refresh_bounty_card(&w.sync(), 4).await.unwrap();
    let body = &w.gh.comments()[0].body;
    assert!(
        !body.contains("[click me](") && body.contains("\\[click me\\]\\(") && body.contains("\\<img") && body.contains("\\@everyone"),
        "{body}"
    );
}

// ---------------- the "Paid" announcement ----------------

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_paid_comment_names_the_contributor_and_links_the_transaction_once(pool: PgPool) {
    let w = world(pool.clone()).await;
    open_bounty(&w, 4, 3).await;
    prs::upsert(
        &pool,
        &prs::PrUpsert { id: 100, number: 7, author_github_id: 900, author_login: "dev", head_sha: "a", state: "merged", merged_at: None },
    )
    .await
    .unwrap();
    prs::link(&pool, 100, 4).await.unwrap();
    payouts::create_pending(&pool, 4, 7, 900).await.unwrap();

    assert!(!announce_payout(&w.sync(), 4).await.unwrap(), "nothing to announce while the bounty is open");
    bounties::apply_released(&pool, 4, "e9", &"ab".repeat(32), "GWIN", Some(900)).await.unwrap();
    assert!(announce_payout(&w.sync(), 4).await.unwrap());
    let c = &w.gh.comments()[0];
    assert_eq!(c.issue, 7, "on the merged PR");
    assert!(c.body.starts_with("**Paid 4.85 XLM to @dev.**") && c.body.contains("](https://explorer.example/tx/abababab"), "{}", c.body);
    assert!(!announce_payout(&w.sync(), 4).await.unwrap(), "a replayed event never comments twice");
    assert_eq!(w.gh.comments().len(), 1);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn without_a_payout_row_the_announcement_goes_on_the_issue_with_a_short_address(pool: PgPool) {
    let w = world(pool.clone()).await;
    open_bounty(&w, 4, 3).await;
    bounties::apply_released(&pool, 4, "e9", "tx", "GABCDEFGHIJKLMNOPQRSTUVWXYZ234567ABCDEFGHIJKLMNOPQRSTUVW", None).await.unwrap();
    announce_payout(&w.sync(), 4).await.unwrap();
    let c = &w.gh.comments()[0];
    assert_eq!(c.issue, 3);
    assert!(c.body.contains("`GABCDE…TUVW`"), "{}", c.body);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn the_notifier_comments_on_the_pr_or_falls_back_to_the_issue(pool: PgPool) {
    let w = world(pool.clone()).await;
    open_bounty(&w, 4, 3).await;
    let n = GithubNotifier { db: w.db.clone(), gh: w.gh.clone() };
    n.notify(4, 7, "on the PR").await.unwrap();
    n.notify(4, 0, "on the issue").await.unwrap();
    n.notify(999, 7, "unknown bounty: nothing happens").await.unwrap();
    assert_eq!(w.gh.comments().iter().map(|c| (c.issue, c.body.as_str())).collect::<Vec<_>>(), [(7, "on the PR"), (3, "on the issue")]);
}

// ---------------- refund reminders ----------------

#[sqlx::test(migrator = "MIGRATOR")]
async fn refund_reminders_go_out_once_each_at_the_right_time(pool: PgPool) {
    let w = world(pool.clone()).await;
    open_bounty(&w, 4, 3).await;
    let commenter = GithubCommenter { db: w.db.clone(), gh: w.gh.clone() };
    let grace = Duration::hours(72);
    let deadline = bounties::get(&pool, 4).await.unwrap().unwrap().deadline;
    let send = |at| send_refund_reminders(&w.db, &commenter, grace, "https://app.example", at);

    assert!(send(Utc::now()).await.unwrap().is_empty(), "weeks before the window: silent");
    let soon = deadline + grace - Duration::hours(47);
    assert_eq!(send(soon).await.unwrap(), ["soon:4"]);
    assert!(send(soon + Duration::minutes(5)).await.unwrap().is_empty(), "not twice");
    let open = deadline + grace + Duration::minutes(1);
    assert_eq!(send(open).await.unwrap(), ["open:4"]);
    assert!(send(open + Duration::hours(5)).await.unwrap().is_empty());
    let b = w.gh.bodies();
    assert!(b[0].contains("opens in under 48 hours") && b[1].contains("is now open"));
    // closed bounties get none
    bounties::apply_refunded(&pool, 4, "e", "t").await.unwrap();
    assert!(send(open + Duration::days(9)).await.unwrap().is_empty());
}

struct FlakyCommenter(Mutex<u32>);
#[async_trait]
impl IssueCommenter for FlakyCommenter {
    async fn comment(&self, _b: i64, _body: &str) -> Result<(), escrow_engine::EngineError> {
        let mut n = self.0.lock().unwrap();
        *n += 1;
        if *n == 1 { Err(GithubError::Status { status: 502, what: "x".into() }.into()) } else { Ok(()) }
    }
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_reminder_whose_comment_failed_is_retried_not_forgotten(pool: PgPool) {
    let w = world(pool.clone()).await;
    open_bounty(&w, 4, 3).await;
    let deadline = bounties::get(&pool, 4).await.unwrap().unwrap().deadline;
    let c = FlakyCommenter(Mutex::new(0));
    let at = deadline + Duration::hours(72) + Duration::minutes(1);
    assert!(send_refund_reminders(&w.db, &c, Duration::hours(72), "w", at).await.is_err());
    assert!(!audit::exists(&pool, "reminder.refund_open", "bounty:4").await.unwrap(), "only marked sent after the comment succeeded");
    assert_eq!(send_refund_reminders(&w.db, &c, Duration::hours(72), "w", at).await.unwrap(), ["open:4"]);
}

// ---------------- installations backfill ----------------

#[sqlx::test(migrator = "MIGRATOR")]
async fn backfill_reconciles_installations_and_skips_suspended_ones(pool: PgPool) {
    let w = World::new(pool.clone());
    w.gh.with(|s| {
        s.installations = vec![
            InstallationInfo {
                id: 1,
                account: escrow_github::api::AccountInfo { login: "acme".into(), kind: "Organization".into() },
                suspended_at: None,
            },
            InstallationInfo {
                id: 2,
                account: escrow_github::api::AccountInfo { login: "paused".into(), kind: "User".into() },
                suspended_at: Some(Utc::now()),
            },
        ];
        s.repos = vec![RepoInfo { id: 10, full_name: "acme/widgets".into(), private: false }];
    });
    let (installs, repos) = sync_installations(&w.db, w.gh.as_ref()).await.unwrap();
    assert_eq!((installs, repos), (2, 1), "the suspended installation's repositories are not listed");
    assert!(escrow_db::github::get_installation(&pool, 2).await.unwrap().unwrap().suspended_at.is_some());
}

// ---------------- health and alerts ----------------

struct Down;
#[async_trait]
impl ChainReader for Down {
    async fn get_bounty(&self, _id: u64) -> Result<ChainBounty, ChainError> {
        Err(ChainError::Http("down".into()))
    }
    async fn get_config(&self) -> Result<ContractConfig, ChainError> {
        Err(ChainError::Http("down".into()))
    }
    async fn latest_ledger(&self) -> Result<u32, ChainError> {
        Err(ChainError::Http("down".into()))
    }
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn health_is_ok_when_dependencies_answer_and_the_indexer_keeps_up(pool: PgPool) {
    let w = World::new(pool.clone());
    webhooks::save_cursor(&pool, CURSOR_ID, "c", 9_990).await.unwrap();
    let h = health_report(&w.db, w.chain.as_ref()).await;
    assert!(h.ok && h.db && h.rpc);
    assert_eq!(h.indexer_lag_ledgers, Some(10));

    webhooks::save_cursor(&pool, CURSOR_ID, "c", 9_000).await.unwrap();
    let lagging = health_report(&w.db, w.chain.as_ref()).await;
    assert!(!lagging.ok, "1000 ledgers behind is not healthy");
    assert_eq!(lagging.indexer_lag_ledgers, Some(1000));

    let rpc_down = health_report(&w.db, &Down).await;
    assert!(!rpc_down.ok && !rpc_down.rpc && rpc_down.db);
    let json = serde_json::to_value(&rpc_down).unwrap();
    assert_eq!(json["indexerLagLedgers"], serde_json::Value::Null);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn every_alert_condition_is_reported_and_a_calm_system_is_silent(pool: PgPool) {
    let w = world(pool.clone()).await;
    let calm = AlertInputs { relayer_balance_xlm: Some(500.0), min_relayer_xlm: 50.0, latest_ledger: Some(10_000), now: Utc::now() };
    assert!(collect_alerts(&w.db, &calm).await.unwrap().is_empty());

    open_bounty(&w, 4, 3).await;
    sqlx::query("UPDATE bounties SET deadline = now() + interval '1 day'").execute(&pool).await.unwrap();
    let p = payouts::create_pending(&pool, 4, 7, 900).await.unwrap().unwrap();
    payouts::mark_failed(&pool, &p.id, "boom", true).await.unwrap();
    webhooks::save_cursor(&pool, CURSOR_ID, "c", 9_000).await.unwrap();
    let d = disputes::create(&pool, 4, 500, "x").await.unwrap();
    sqlx::query("UPDATE disputes SET created_at = now() - interval '2 days' WHERE id = $1").bind(&d.id).execute(&pool).await.unwrap();
    jobs::enqueue(&pool, "webhook", &json!({}), Utc::now(), 1, None).await.unwrap();
    let j = jobs::claim(&pool, &["webhook"], 60).await.unwrap().unwrap();
    jobs::fail(&pool, j.id, "x", 1).await.unwrap();

    let all = collect_alerts(
        &w.db,
        &AlertInputs { relayer_balance_xlm: Some(3.0), min_relayer_xlm: 50.0, latest_ledger: Some(10_000), now: Utc::now() },
    )
    .await
    .unwrap();
    let text = all.join("\n");
    for needle in [
        "Relayer balance low: 3.00 XLM",
        "1 payout(s) FAILED",
        "Indexer is 1000 ledgers behind",
        "1 dispute(s) older than 36 h",
        "Bounty 4 has a merged PR (FAILED)",
        "1 background job(s) gave up",
    ] {
        assert!(text.contains(needle), "missing `{needle}` in:\n{text}");
    }
}

#[tokio::test]
async fn an_alert_is_sent_once_per_cooldown() {
    let s = AlertSender::new(None, std::time::Duration::from_secs(3600));
    let t0 = Instant::now();
    let msgs = vec!["relayer low".to_string(), "payout failed".to_string()];
    assert_eq!(s.send(&msgs, t0).await.len(), 2);
    assert!(s.send(&msgs, t0 + std::time::Duration::from_secs(60)).await.is_empty(), "no spam");
    assert_eq!(s.send(&["new problem".to_string()], t0 + std::time::Duration::from_secs(61)).await, ["new problem"]);
    assert_eq!(s.send(&msgs, t0 + std::time::Duration::from_secs(3601)).await.len(), 2, "reminded after the cooldown");
}

// ---------------- the worker ----------------

struct Runner {
    calls: Mutex<Vec<(i64, bool)>>,
    result: Mutex<Option<ReviewFailure>>,
}
#[async_trait]
impl ReviewRunner for Runner {
    async fn run(&self, pr_id: i64, force: bool) -> Result<String, ReviewFailure> {
        self.calls.lock().unwrap().push((pr_id, force));
        self.result.lock().unwrap().take().map_or(Ok("reviewed".into()), Err)
    }
}

fn worker(w: &World, reviews: Option<Arc<dyn ReviewRunner>>) -> Arc<WorkerDeps> {
    Arc::new(WorkerDeps {
        db: w.db.clone(),
        dispatch: w.dispatch(None),
        sync: w.sync(),
        reviews,
        chain: w.chain.clone(),
        relayer: None,
        alerts: Arc::new(AlertSender::new(None, std::time::Duration::from_secs(60))),
        min_relayer_xlm: 50.0,
        grace: Duration::hours(72),
    })
}

async fn until<F: std::future::Future<Output = bool>>(mut f: impl FnMut() -> F) {
    for _ in 0..200 {
        if f().await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("condition not reached in time");
}

async fn status_of(pool: &PgPool, kind: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT status FROM jobs WHERE kind = $1 ORDER BY id").bind(kind).fetch_all(pool).await.unwrap()
}

fn issue_comment_payload(body: &str) -> serde_json::Value {
    json!({ "action": "created", "installation": { "id": 1 }, "repository": { "id": seed::REPO_ID, "full_name": seed::REPO },
        "issue": { "number": 3, "state": "open" }, "comment": { "id": 1, "body": body, "user": { "id": 500, "login": "alice", "type": "User" } } })
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_queued_webhook_is_processed_end_to_end_and_the_delivery_is_marked(pool: PgPool) {
    let w = world(pool.clone()).await;
    webhooks::insert_delivery(&pool, "delivery-1", "issue_comment").await.unwrap();
    kinds::enqueue_webhook(&pool, "delivery-1", "issue_comment", &issue_comment_payload("/bounty help")).await.unwrap();
    let stop = CancellationToken::new();
    let q = tokio::spawn(run_queue(worker(&w, None), stop.clone(), 2, std::time::Duration::from_millis(10)));
    until(|| async { status_of(&pool, "webhook").await == ["done"] }).await;
    stop.cancel();
    q.await.unwrap();
    assert!(w.gh.bodies()[0].contains("SkillEscrow commands"));
    let processed: Option<chrono::DateTime<Utc>> =
        sqlx::query_scalar("SELECT processed_at FROM webhook_deliveries WHERE id = 'delivery-1'").fetch_one(&pool).await.unwrap();
    assert!(processed.is_some());
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_transient_failure_is_retried_later_and_then_succeeds(pool: PgPool) {
    let w = world(pool.clone()).await;
    w.gh.allow("alice", "write");
    webhooks::insert_delivery(&pool, "d2", "issue_comment").await.unwrap();
    kinds::enqueue_webhook(&pool, "d2", "issue_comment", &issue_comment_payload("/bounty help")).await.unwrap();
    w.gh.with(|s| s.fail_next = Some(GithubError::Status { status: 503, what: "x".into() }));
    let job = jobs::claim(&pool, &[kinds::WEBHOOK], 60).await.unwrap().unwrap();
    let wd = worker(&w, None);
    let err = handle_job(&wd, &job).await.unwrap_err();
    assert!(err.is_transient());
    jobs::fail(&pool, job.id, &err.to_string(), 0).await.unwrap();
    assert_eq!(status_of(&pool, "webhook").await, ["queued"]);
    let again = jobs::claim(&pool, &[kinds::WEBHOOK], 60).await.unwrap().unwrap();
    assert_eq!(again.attempts, 2);
    assert!(handle_job(&wd, &again).await.is_ok());
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_job_that_can_never_succeed_is_parked_dead_with_its_reason_and_the_delivery_records_it(pool: PgPool) {
    let w = world(pool.clone()).await;
    webhooks::insert_delivery(&pool, "d3", "pull_request").await.unwrap();
    kinds::enqueue_webhook(&pool, "d3", "pull_request", &json!({ "action": "closed" })).await.unwrap();
    let stop = CancellationToken::new();
    let q = tokio::spawn(run_queue(worker(&w, None), stop.clone(), 1, std::time::Duration::from_millis(10)));
    until(|| async { status_of(&pool, "webhook").await == ["dead"] }).await;
    stop.cancel();
    q.await.unwrap();
    let (err, attempts): (Option<String>, i32) =
        sqlx::query_as("SELECT last_error, attempts FROM jobs WHERE kind = 'webhook'").fetch_one(&pool).await.unwrap();
    assert!(err.unwrap().contains("bad webhook payload"));
    assert_eq!(attempts, 1, "no pointless retries");
    let delivery_err: Option<String> =
        sqlx::query_scalar("SELECT error FROM webhook_deliveries WHERE id = 'd3'").fetch_one(&pool).await.unwrap();
    assert!(delivery_err.is_some());
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn many_jobs_across_several_workers_each_run_exactly_once(pool: PgPool) {
    let w = world(pool.clone()).await;
    for n in 0..25 {
        kinds::enqueue_webhook(&pool, &format!("d-{n}"), "ping", &json!({})).await.unwrap();
        webhooks::insert_delivery(&pool, &format!("d-{n}"), "ping").await.unwrap();
    }
    let stop = CancellationToken::new();
    let q = tokio::spawn(run_queue(worker(&w, None), stop.clone(), 6, std::time::Duration::from_millis(5)));
    until(|| async { jobs::count_pending(&pool).await.unwrap() == 0 }).await;
    stop.cancel();
    q.await.unwrap();
    let attempts: Vec<i32> = sqlx::query_scalar("SELECT attempts FROM jobs").fetch_all(&pool).await.unwrap();
    assert_eq!((attempts.len(), attempts.iter().all(|a| *a == 1)), (25, true));
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn review_jobs_run_only_when_a_reviewer_is_configured(pool: PgPool) {
    let w = world(pool.clone()).await;
    kinds::enqueue_review(&pool, 100, std::time::Duration::ZERO, true).await.unwrap();
    let stop = CancellationToken::new();
    let q = tokio::spawn(run_queue(worker(&w, None), stop.clone(), 1, std::time::Duration::from_millis(10)));
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    stop.cancel();
    q.await.unwrap();
    assert_eq!(status_of(&pool, "review").await, ["queued"], "left for a worker that has an API key");

    let runner = Arc::new(Runner { calls: Mutex::default(), result: Mutex::default() });
    let stop = CancellationToken::new();
    let q = tokio::spawn(run_queue(worker(&w, Some(runner.clone())), stop.clone(), 1, std::time::Duration::from_millis(10)));
    until(|| async { status_of(&pool, "review").await == ["done"] }).await;
    stop.cancel();
    q.await.unwrap();
    assert_eq!(*runner.calls.lock().unwrap(), [(100, true)]);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn reviews_are_debounced_so_a_burst_of_pushes_costs_one_job(pool: PgPool) {
    for _ in 0..5 {
        kinds::enqueue_review(&pool, 100, std::time::Duration::from_secs(300), false).await.unwrap();
    }
    assert_eq!(status_of(&pool, "review").await, ["queued"]);
    let (due,): (bool,) = sqlx::query_as("SELECT run_at > now() + interval '4 minutes' FROM jobs").fetch_one(&pool).await.unwrap();
    assert!(due, "waits for the quiet period");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_failing_review_is_retryable_only_when_the_failure_is_transient(pool: PgPool) {
    let w = world(pool.clone()).await;
    let runner =
        Arc::new(Runner { calls: Mutex::default(), result: Mutex::new(Some(ReviewFailure { message: "529".into(), transient: true })) });
    let wd = worker(&w, Some(runner.clone()));
    kinds::enqueue_review(&pool, 100, std::time::Duration::ZERO, false).await.unwrap();
    let job = jobs::claim(&pool, &[kinds::REVIEW], 60).await.unwrap().unwrap();
    assert!(handle_job(&wd, &job).await.unwrap_err().is_transient());
    *runner.result.lock().unwrap() = Some(ReviewFailure { message: "bad".into(), transient: false });
    assert!(!handle_job(&wd, &job).await.unwrap_err().is_transient());
    assert!(handle_job(&worker(&w, None), &job).await.is_err(), "no reviewer: an error, not a silent success");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn card_and_announce_jobs_validate_their_payload(pool: PgPool) {
    let w = world(pool.clone()).await;
    open_bounty(&w, 4, 3).await;
    kinds::enqueue_card_refresh(&pool, 4).await.unwrap();
    kinds::enqueue_card_refresh(&pool, 4).await.unwrap();
    assert_eq!(status_of(&pool, "card.refresh").await.len(), 1, "duplicates collapse while queued");
    let job = jobs::claim(&pool, &[kinds::CARD_REFRESH], 60).await.unwrap().unwrap();
    assert!(handle_job(&worker(&w, None), &job).await.unwrap().starts_with("card Posted"));

    let mut bad = job.clone();
    bad.payload = json!({});
    assert!(matches!(handle_job(&worker(&w, None), &bad).await, Err(escrow_engine::EngineError::Payload(_))));
    let mut unknown = job;
    unknown.kind = "mystery".into();
    assert!(handle_job(&worker(&w, None), &unknown).await.is_err());
    let _ = (BountyStatus::Open, prs::get(&pool, 1).await);
}
