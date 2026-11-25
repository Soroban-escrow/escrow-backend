//! The payout state machine against real Postgres, with a fake chain, relayer and notifier.

use async_trait::async_trait;
use chrono::{Duration, Utc};
use escrow_chain::{ChainBounty, ChainError, ChainReader, ContractConfig, ReleaseSigner, TxOutcome};
use escrow_core::BountyStatus;
use escrow_db::{Db, MIGRATOR, bounties, github, payouts, users};
use escrow_payouts::{
    Advance, MAX_ATTEMPTS, MergeOutcome, Notifier, PayoutDeps, PayoutError, RelayResult, RetryTable, advance, on_merge, on_wallet_linked,
    relay_release, relayer_pass,
};
use sqlx::PgPool;
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::Instant,
};

const MAINTAINER: &str = "GMAINTAINER";
const WINNER: &str = "GWINNER";

// ---- fakes ----

#[derive(Default)]
struct FakeChain(Mutex<HashMap<u64, ChainBounty>>);

impl FakeChain {
    fn put(&self, id: u64, status: BountyStatus, relayer_allowed: bool, winner: Option<&str>) {
        self.0.lock().unwrap().insert(
            id,
            ChainBounty {
                id,
                maintainer: MAINTAINER.into(),
                repo_id: 10,
                issue: 3,
                token: "CTOKEN".into(),
                amount: 100,
                fee_bps: 300,
                deadline: 0,
                relayer_allowed,
                status,
                winner: winner.map(Into::into),
                created_at: 0,
            },
        );
    }
}

#[async_trait]
impl ChainReader for FakeChain {
    async fn get_bounty(&self, id: u64) -> Result<ChainBounty, ChainError> {
        self.0.lock().unwrap().get(&id).cloned().ok_or_else(|| ChainError::Simulation("Error(Contract, #10)".into()))
    }
    async fn get_config(&self) -> Result<ContractConfig, ChainError> {
        Err(ChainError::Invalid("unused".into()))
    }
    async fn latest_ledger(&self) -> Result<u32, ChainError> {
        Ok(1)
    }
}

#[derive(Default)]
struct Signer {
    submits: Mutex<VecDeque<Result<String, ChainError>>>,
    waits: Mutex<VecDeque<Result<TxOutcome, ChainError>>>,
    submitted: Mutex<Vec<(u64, String)>>,
    waited: Mutex<Vec<String>>,
    /// what the database said the payout's hash was at the moment we started waiting
    hash_in_db_during_wait: Mutex<Option<Option<String>>>,
    db: Mutex<Option<Db>>,
}

#[async_trait]
impl ReleaseSigner for Signer {
    fn address(&self) -> String {
        "GRELAYER".into()
    }
    async fn submit_release(&self, bounty_id: u64, winner: &str) -> Result<String, ChainError> {
        self.submitted.lock().unwrap().push((bounty_id, winner.to_string()));
        self.submits.lock().unwrap().pop_front().unwrap_or_else(|| Ok(format!("hash-{}", self.submitted.lock().unwrap().len())))
    }
    async fn wait_for_tx(&self, hash: &str) -> Result<TxOutcome, ChainError> {
        self.waited.lock().unwrap().push(hash.to_string());
        let db = self.db.lock().unwrap().clone();
        if let Some(db) = db {
            let first = self.hash_in_db_during_wait.lock().unwrap().is_none();
            if first {
                let p = payouts::for_bounty(&db.pool, 4).await.unwrap().unwrap();
                *self.hash_in_db_during_wait.lock().unwrap() = Some(p.tx_hash);
            }
        }
        self.waits.lock().unwrap().pop_front().unwrap_or(Ok(TxOutcome::Success))
    }
    async fn balance_xlm(&self) -> Result<f64, ChainError> {
        Ok(100.0)
    }
}

#[derive(Default)]
struct Notes(Mutex<Vec<String>>);
#[async_trait]
impl Notifier for Notes {
    async fn notify(&self, _b: i64, _pr: i32, m: &str) -> Result<(), String> {
        self.0.lock().unwrap().push(m.to_string());
        Ok(())
    }
}
impl Notes {
    fn all(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

struct Env {
    d: PayoutDeps,
    chain: Arc<FakeChain>,
    notes: Arc<Notes>,
    signer: Arc<Signer>,
    pool: PgPool,
}

async fn env(pool: PgPool) -> Env {
    github::upsert_installation(&pool, 1, "acme", "Organization", None).await.unwrap();
    github::upsert_repo(&pool, 10, 1, "acme/widgets", false).await.unwrap();
    bounties::insert(
        &pool,
        &bounties::NewBounty {
            id: 4,
            repo_id: 10,
            issue_number: 3,
            issue_url: "u",
            maintainer: MAINTAINER,
            maintainer_github_id: Some(500),
            token: "CTOKEN",
            amount: 100,
            fee_bps: 300,
            deadline: Utc::now() + Duration::days(7),
            relayer_allowed: true,
            fund_tx: "f",
            created_at: Utc::now(),
            last_event_id: "e1",
        },
    )
    .await
    .unwrap();
    let chain = Arc::new(FakeChain::default());
    chain.put(4, BountyStatus::Open, true, None);
    let notes = Arc::new(Notes::default());
    let signer = Arc::new(Signer::default());
    *signer.db.lock().unwrap() = Some(Db::from_pool(pool.clone()));
    let d = PayoutDeps {
        db: Db::from_pool(pool.clone()),
        chain: chain.clone(),
        notifier: notes.clone(),
        web_url: "https://app.example".into(),
    };
    Env { d, chain, notes, signer, pool }
}

async fn contributor(pool: &PgPool, github_id: i64, wallet: Option<&str>) {
    let u = users::upsert_github(pool, github_id, "dev", None, false).await.unwrap();
    if let Some(w) = wallet {
        users::set_wallet(pool, &u.id, w, Utc::now()).await.unwrap();
    }
}

async fn status(pool: &PgPool) -> String {
    payouts::for_bounty(pool, 4).await.unwrap().unwrap().status
}

// ---- merge handling ----

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_merge_with_a_linked_wallet_and_relayer_opt_in_is_queued(pool: PgPool) {
    let e = env(pool.clone()).await;
    contributor(&pool, 900, Some(WINNER)).await;
    assert_eq!(on_merge(&e.d, 4, 7, 900).await.unwrap(), MergeOutcome::Created);
    let p = payouts::for_bounty(&pool, 4).await.unwrap().unwrap();
    assert_eq!((p.status.as_str(), p.winner_address.as_deref(), p.pr_number), ("QUEUED", Some(WINNER), 7));
    assert!(e.notes.all().is_empty());
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_merge_without_a_wallet_waits_and_asks_for_one_then_continues_when_linked(pool: PgPool) {
    let e = env(pool.clone()).await;
    contributor(&pool, 900, None).await;
    on_merge(&e.d, 4, 7, 900).await.unwrap();
    assert_eq!(status(&pool).await, "PENDING_WALLET");
    assert_eq!(e.notes.all(), ["Link your wallet at https://app.example/me to receive your payout."]);

    // the contributor links a wallet later
    let u = users::by_github_id(&pool, 900).await.unwrap().unwrap();
    users::set_wallet(&pool, &u.id, WINNER, Utc::now()).await.unwrap();
    assert_eq!(on_wallet_linked(&e.d, 900).await.unwrap(), 1);
    assert_eq!(status(&pool).await, "QUEUED");
    assert_eq!(on_wallet_linked(&e.d, 900).await.unwrap(), 0, "nothing left waiting");
    assert_eq!(on_wallet_linked(&e.d, 12345).await.unwrap(), 0);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn without_relayer_opt_in_the_maintainer_must_sign(pool: PgPool) {
    let e = env(pool.clone()).await;
    e.chain.put(4, BountyStatus::Open, false, None);
    contributor(&pool, 900, Some(WINNER)).await;
    on_merge(&e.d, 4, 7, 900).await.unwrap();
    assert_eq!(status(&pool).await, "AWAITING_MAINTAINER");
    assert_eq!(e.notes.all(), ["The maintainer must release this bounty at https://app.example/b/4."]);
    assert!(payouts::due(&pool, 10).await.unwrap().is_empty(), "the relayer never touches it");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn the_first_merged_pr_wins_and_a_second_is_told_so(pool: PgPool) {
    let e = env(pool.clone()).await;
    contributor(&pool, 900, Some(WINNER)).await;
    contributor(&pool, 901, Some("GOTHER")).await;
    assert_eq!(on_merge(&e.d, 4, 7, 900).await.unwrap(), MergeOutcome::Created);
    assert_eq!(on_merge(&e.d, 4, 8, 901).await.unwrap(), MergeOutcome::Duplicate);
    assert!(e.notes.all()[0].contains("already claimed"));
    assert_eq!(payouts::for_bounty(&pool, 4).await.unwrap().unwrap().winner_github_id, 900);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_bounty_the_chain_says_is_closed_never_gets_a_payout(pool: PgPool) {
    let e = env(pool.clone()).await;
    contributor(&pool, 900, Some(WINNER)).await;
    for st in [BountyStatus::Released, BountyStatus::Refunded, BountyStatus::Disputed, BountyStatus::Resolved] {
        e.chain.put(4, st, true, None);
        assert_eq!(on_merge(&e.d, 4, 7, 900).await.unwrap(), MergeOutcome::NotOpen, "{st:?}");
    }
    assert!(payouts::for_bounty(&pool, 4).await.unwrap().is_none());
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_winner_wallet_equal_to_the_maintainer_fails_the_payout(pool: PgPool) {
    let e = env(pool.clone()).await;
    contributor(&pool, 900, Some(MAINTAINER)).await;
    on_merge(&e.d, 4, 7, 900).await.unwrap();
    let p = payouts::for_bounty(&pool, 4).await.unwrap().unwrap();
    assert_eq!((p.status.as_str(), p.last_error.as_deref()), ("FAILED", Some("winner is the maintainer")));
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn concurrent_advances_move_the_payout_exactly_once(pool: PgPool) {
    let e = env(pool.clone()).await;
    e.chain.put(4, BountyStatus::Open, false, None);
    contributor(&pool, 900, None).await;
    on_merge(&e.d, 4, 7, 900).await.unwrap();
    let u = users::by_github_id(&pool, 900).await.unwrap().unwrap();
    users::set_wallet(&pool, &u.id, WINNER, Utc::now()).await.unwrap();
    e.notes.0.lock().unwrap().clear();
    let mut tasks = vec![];
    for _ in 0..8 {
        let d = e.d.clone();
        tasks.push(tokio::spawn(async move { advance(&d, 4).await.unwrap() }));
    }
    let mut results = vec![];
    for t in tasks {
        results.push(t.await.unwrap());
    }
    assert_eq!(results.iter().filter(|r| **r == Advance::AwaitingMaintainer).count(), 1);
    assert_eq!(e.notes.all().len(), 1, "the maintainer is notified once, not eight times");
}

// ---- the relayer ----

async fn queued(e: &Env) {
    contributor(&e.pool, 900, Some(WINNER)).await;
    on_merge(&e.d, 4, 7, 900).await.unwrap();
    assert_eq!(status(&e.pool).await, "QUEUED");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_release_is_submitted_waited_on_and_confirmed_with_the_hash_stored_first(pool: PgPool) {
    let e = env(pool.clone()).await;
    queued(&e).await;
    assert_eq!(relay_release(&e.d, &*e.signer, 4).await.unwrap(), RelayResult::Confirmed);
    let p = payouts::for_bounty(&pool, 4).await.unwrap().unwrap();
    assert_eq!((p.status.as_str(), p.tx_hash.as_deref(), p.attempts), ("CONFIRMED", Some("hash-1"), 1));
    assert_eq!(*e.signer.submitted.lock().unwrap(), [(4, WINNER.to_string())]);
    assert_eq!(
        *e.signer.hash_in_db_during_wait.lock().unwrap(),
        Some(Some("hash-1".to_string())),
        "the hash was already in the database while we waited"
    );
    // asking again is a no-op, never a second payment
    assert_eq!(relay_release(&e.d, &*e.signer, 4).await.unwrap(), RelayResult::Confirmed);
    assert_eq!(e.signer.submitted.lock().unwrap().len(), 1);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_crash_after_submitting_resumes_by_waiting_not_by_submitting_again(pool: PgPool) {
    let e = env(pool.clone()).await;
    queued(&e).await;
    // the wait fails the first time (as if the process died mid-wait: we get an error and the row stays SUBMITTED)
    e.signer.waits.lock().unwrap().push_back(Err(ChainError::Http("timeout".into())));
    assert!(relay_release(&e.d, &*e.signer, 4).await.is_err());
    assert_eq!(status(&pool).await, "SUBMITTED");
    assert_eq!(relay_release(&e.d, &*e.signer, 4).await.unwrap(), RelayResult::Confirmed);
    assert_eq!(e.signer.submitted.lock().unwrap().len(), 1, "exactly one submission, ever");
    assert_eq!(*e.signer.waited.lock().unwrap(), ["hash-1", "hash-1"]);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn if_the_release_already_landed_the_chain_is_believed_and_nothing_is_submitted(pool: PgPool) {
    let e = env(pool.clone()).await;
    queued(&e).await;
    e.chain.put(4, BountyStatus::Released, true, Some(WINNER));
    assert_eq!(relay_release(&e.d, &*e.signer, 4).await.unwrap(), RelayResult::Confirmed);
    assert!(e.signer.submitted.lock().unwrap().is_empty());
    assert_eq!(status(&pool).await, "CONFIRMED");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_bounty_that_changed_under_us_fails_the_payout_without_submitting(pool: PgPool) {
    let e = env(pool.clone()).await;
    queued(&e).await;
    e.chain.put(4, BountyStatus::Released, true, Some("GSOMEONEELSE"));
    assert_eq!(relay_release(&e.d, &*e.signer, 4).await.unwrap(), RelayResult::Failed);
    assert!(e.signer.submitted.lock().unwrap().is_empty());
    assert!(payouts::for_bounty(&pool, 4).await.unwrap().unwrap().last_error.unwrap().contains("RELEASED"));
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_contract_rejection_fails_at_once_without_retrying(pool: PgPool) {
    let e = env(pool.clone()).await;
    queued(&e).await;
    e.signer.submits.lock().unwrap().push_back(Err(ChainError::Simulation("HostError: Error(Contract, #17)".into())));
    assert_eq!(relay_release(&e.d, &*e.signer, 4).await.unwrap(), RelayResult::Failed);
    let p = payouts::for_bounty(&pool, 4).await.unwrap().unwrap();
    assert_eq!((p.status.as_str(), p.attempts, p.tx_hash), ("FAILED", 1, None));
    assert!(p.last_error.unwrap().contains("#17"));
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn transient_failures_retry_then_give_up_after_the_limit(pool: PgPool) {
    let e = env(pool.clone()).await;
    queued(&e).await;
    for attempt in 1..MAX_ATTEMPTS {
        e.signer.submits.lock().unwrap().push_back(Err(ChainError::Busy));
        let r = relay_release(&e.d, &*e.signer, 4).await;
        assert!(matches!(r, Err(PayoutError::Transient(_))), "attempt {attempt}");
        assert_eq!(status(&pool).await, "QUEUED", "still retryable");
    }
    e.signer.submits.lock().unwrap().push_back(Err(ChainError::Busy));
    assert_eq!(relay_release(&e.d, &*e.signer, 4).await.unwrap(), RelayResult::Failed);
    let p = payouts::for_bounty(&pool, 4).await.unwrap().unwrap();
    assert_eq!((p.status.as_str(), p.attempts), ("FAILED", MAX_ATTEMPTS));
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_transaction_that_failed_on_chain_fails_the_payout(pool: PgPool) {
    let e = env(pool.clone()).await;
    queued(&e).await;
    e.signer.waits.lock().unwrap().push_back(Ok(TxOutcome::Failed));
    assert_eq!(relay_release(&e.d, &*e.signer, 4).await.unwrap(), RelayResult::Failed);
    assert_eq!(payouts::for_bounty(&pool, 4).await.unwrap().unwrap().last_error.as_deref(), Some("transaction failed on chain"));
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_just_submitted_transaction_that_is_not_visible_yet_is_waited_on_not_resubmitted(pool: PgPool) {
    let e = env(pool.clone()).await;
    queued(&e).await;
    e.signer.waits.lock().unwrap().push_back(Ok(TxOutcome::NotFound));
    assert!(matches!(relay_release(&e.d, &*e.signer, 4).await, Err(PayoutError::Transient(_))));
    assert_eq!(status(&pool).await, "SUBMITTED");
    assert_eq!(e.signer.submitted.lock().unwrap().len(), 1);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn an_expired_unseen_transaction_is_resubmitted_only_when_the_chain_is_still_open(pool: PgPool) {
    let e = env(pool.clone()).await;
    queued(&e).await;
    e.signer.waits.lock().unwrap().push_back(Ok(TxOutcome::NotFound));
    let _ = relay_release(&e.d, &*e.signer, 4).await;
    // make the submission old enough that it can no longer land
    sqlx::query("UPDATE payouts SET updated_at = now() - interval '10 minutes'").execute(&pool).await.unwrap();
    e.signer.waits.lock().unwrap().push_back(Ok(TxOutcome::NotFound));
    assert!(matches!(relay_release(&e.d, &*e.signer, 4).await, Err(PayoutError::Transient(_))));
    let p = payouts::for_bounty(&pool, 4).await.unwrap().unwrap();
    assert_eq!((p.status.as_str(), p.tx_hash), ("QUEUED", None), "hash forgotten, ready to resubmit");
    assert_eq!(relay_release(&e.d, &*e.signer, 4).await.unwrap(), RelayResult::Confirmed);
    assert_eq!(e.signer.submitted.lock().unwrap().len(), 2);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn an_expired_transaction_that_actually_landed_is_confirmed_not_resubmitted(pool: PgPool) {
    let e = env(pool.clone()).await;
    queued(&e).await;
    e.signer.waits.lock().unwrap().push_back(Ok(TxOutcome::NotFound));
    let _ = relay_release(&e.d, &*e.signer, 4).await;
    sqlx::query("UPDATE payouts SET updated_at = now() - interval '10 minutes'").execute(&pool).await.unwrap();
    e.chain.put(4, BountyStatus::Released, true, Some(WINNER));
    e.signer.waits.lock().unwrap().push_back(Ok(TxOutcome::NotFound));
    assert_eq!(relay_release(&e.d, &*e.signer, 4).await.unwrap(), RelayResult::Confirmed);
    assert_eq!(e.signer.submitted.lock().unwrap().len(), 1);
}

// ---- the loop ----

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_pass_pays_queued_payouts_and_backs_off_failures(pool: PgPool) {
    let e = env(pool.clone()).await;
    queued(&e).await;
    let mut retry = RetryTable::new();
    let t0 = Instant::now();

    e.signer.submits.lock().unwrap().push_back(Err(ChainError::Busy));
    assert_eq!(relayer_pass(&e.d, &*e.signer, &mut retry, t0).await.unwrap(), 1);
    assert_eq!(retry.len(), 1, "the failed payout is backing off");

    // too soon: skipped
    assert_eq!(relayer_pass(&e.d, &*e.signer, &mut retry, t0 + std::time::Duration::from_secs(1)).await.unwrap(), 0);
    // after the backoff: tried again and paid
    assert_eq!(relayer_pass(&e.d, &*e.signer, &mut retry, t0 + std::time::Duration::from_secs(60)).await.unwrap(), 1);
    assert!(retry.is_empty());
    assert_eq!(status(&pool).await, "CONFIRMED");
    assert_eq!(
        relayer_pass(&e.d, &*e.signer, &mut retry, t0 + std::time::Duration::from_secs(120)).await.unwrap(),
        0,
        "nothing left to do"
    );
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn one_bad_payout_does_not_block_the_others(pool: PgPool) {
    let e = env(pool.clone()).await;
    queued(&e).await;
    // a second bounty, queued too
    bounties::insert(
        &pool,
        &bounties::NewBounty {
            id: 5,
            repo_id: 10,
            issue_number: 9,
            issue_url: "u",
            maintainer: MAINTAINER,
            maintainer_github_id: None,
            token: "CTOKEN",
            amount: 1,
            fee_bps: 300,
            deadline: Utc::now() + Duration::days(1),
            relayer_allowed: true,
            fund_tx: "f",
            created_at: Utc::now(),
            last_event_id: "e",
        },
    )
    .await
    .unwrap();
    e.chain.put(5, BountyStatus::Open, true, None);
    on_merge(&e.d, 5, 11, 900).await.unwrap();
    e.signer.submits.lock().unwrap().push_back(Err(ChainError::Busy)); // the first one stumbles
    let mut retry = RetryTable::new();
    assert_eq!(relayer_pass(&e.d, &*e.signer, &mut retry, Instant::now()).await.unwrap(), 2);
    let paid = payouts::by_status(&pool, "CONFIRMED", 10).await.unwrap();
    assert_eq!(paid.len(), 1, "the second was paid despite the first failing");
}
