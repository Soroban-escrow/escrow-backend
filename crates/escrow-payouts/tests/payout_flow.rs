//! The payout state machine against real Postgres, with a fake chain, relayer and notifier.

use async_trait::async_trait;
use chrono::{Duration, Utc};
use escrow_chain::{ChainBounty, ChainError, ChainReader, ContractConfig, ReleaseSigner, TxOutcome};
use escrow_core::{BountyStatus, PayoutStatus};
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
async fn busy_signer_records_attempt_and_leaves_payout_queued(pool: PgPool) {
    let e = env(pool.clone()).await;
    queued(&e).await;
    e.signer.submits.lock().unwrap().push_back(Err(ChainError::Busy));
    let r = relay_release(&e.d, &*e.signer, 4).await;
    assert!(matches!(r, Err(PayoutError::Transient(_))));
    let p = payouts::for_bounty(&pool, 4).await.unwrap().unwrap();
    assert_eq!((p.status.as_str(), p.attempts, p.tx_hash), ("QUEUED", 1, None));
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

// ---- #106: settle when the bounty moved under us while the release was in flight ----

/// #106: NotFound + stale + chain says `Refunded` must fail the payout and name the status.
/// This is the branch where the maintainer or arbiter acted between submit and wait: the
/// contract is now closed, our transaction can never land, and the "bounty became X" message
/// is the operator's only breadcrumb back to why.
#[sqlx::test(migrator = "MIGRATOR")]
async fn an_expired_unseen_transaction_fails_when_the_bounty_was_refunded_meanwhile(pool: PgPool) {
    let e = env(pool.clone()).await;
    queued(&e).await;
    e.signer.waits.lock().unwrap().push_back(Ok(TxOutcome::NotFound));
    let _ = relay_release(&e.d, &*e.signer, 4).await;
    sqlx::query("UPDATE payouts SET updated_at = now() - interval '10 minutes'").execute(&pool).await.unwrap();
    e.chain.put(4, BountyStatus::Refunded, true, None);
    e.signer.waits.lock().unwrap().push_back(Ok(TxOutcome::NotFound));

    assert_eq!(relay_release(&e.d, &*e.signer, 4).await.unwrap(), RelayResult::Failed);
    let p = payouts::for_bounty(&pool, 4).await.unwrap().unwrap();
    assert_eq!(p.status.as_str(), "FAILED");
    let msg = p.last_error.as_deref().unwrap_or("");
    assert!(msg.contains("REFUNDED") || msg.contains("Refunded"), "status in message: {msg:?}");
    // No second submission: the chain told us it was over.
    assert_eq!(e.signer.submitted.lock().unwrap().len(), 1);
}

/// #106: same as above but the bounty went to `Disputed`. The payout should fail and the
/// message should carry the new state so the operator can route to the arbiter.
#[sqlx::test(migrator = "MIGRATOR")]
async fn an_expired_unseen_transaction_fails_when_the_bounty_was_disputed_meanwhile(pool: PgPool) {
    let e = env(pool.clone()).await;
    queued(&e).await;
    e.signer.waits.lock().unwrap().push_back(Ok(TxOutcome::NotFound));
    let _ = relay_release(&e.d, &*e.signer, 4).await;
    sqlx::query("UPDATE payouts SET updated_at = now() - interval '10 minutes'").execute(&pool).await.unwrap();
    e.chain.put(4, BountyStatus::Disputed, true, None);
    e.signer.waits.lock().unwrap().push_back(Ok(TxOutcome::NotFound));

    assert_eq!(relay_release(&e.d, &*e.signer, 4).await.unwrap(), RelayResult::Failed);
    let p = payouts::for_bounty(&pool, 4).await.unwrap().unwrap();
    assert_eq!(p.status.as_str(), "FAILED");
    let msg = p.last_error.as_deref().unwrap_or("");
    assert!(msg.contains("DISPUTED") || msg.contains("Disputed"), "status in message: {msg:?}");
    assert_eq!(e.signer.submitted.lock().unwrap().len(), 1);
}

/// #106: NotFound *before* the stale threshold must only bump attempts and keep SUBMITTED.
/// We must not read the chain, must not requeue, must not fail - the transaction may still land.
#[sqlx::test(migrator = "MIGRATOR")]
async fn a_pre_stale_notfound_bumps_attempts_and_stays_submitted(pool: PgPool) {
    let e = env(pool.clone()).await;
    queued(&e).await;
    e.signer.waits.lock().unwrap().push_back(Ok(TxOutcome::NotFound));
    assert!(matches!(relay_release(&e.d, &*e.signer, 4).await, Err(PayoutError::Transient(_))));

    let p = payouts::for_bounty(&pool, 4).await.unwrap().unwrap();
    assert_eq!(p.status.as_str(), "SUBMITTED");
    // One submission (attempts starts at 1), one pre-stale NotFound bumps it to 2.
    assert_eq!(p.attempts, 2);
    assert!(p.tx_hash.is_some(), "hash preserved, nothing to resubmit yet");
    let last = p.last_error.as_deref().unwrap_or("");
    assert!(last.contains("not seen yet"), "last_error records the wait failure: {last:?}");
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

// ---- #107: the contributor sees exactly one "needs attention" comment ----

const FAILURE_MARKER: &str = "This payout could not be completed";

fn failure_comments(notes: &Notes) -> Vec<String> {
    notes.all().into_iter().filter(|m| m.contains(FAILURE_MARKER)).collect()
}

/// #107: a deterministic contract rejection (the contract said no) posts the generic failure
/// comment once. A second pass through `relay_release` is a no-op and must not re-comment.
#[sqlx::test(migrator = "MIGRATOR")]
async fn a_deterministic_rejection_tells_the_contributor_once(pool: PgPool) {
    let e = env(pool.clone()).await;
    queued(&e).await;
    e.notes.0.lock().unwrap().clear();
    e.signer.submits.lock().unwrap().push_back(Err(ChainError::Simulation("HostError: Error(Contract, #17)".into())));

    assert_eq!(relay_release(&e.d, &*e.signer, 4).await.unwrap(), RelayResult::Failed);
    let c = failure_comments(&e.notes);
    assert_eq!(c.len(), 1, "exactly one failure comment: {c:?}");
    // Nothing internal leaks: not the contract code, not any ChainError text.
    assert!(!c[0].contains("#17"), "internal rejection code must not leak: {}", c[0]);
    assert!(!c[0].to_ascii_lowercase().contains("hosterror"));
    assert!(c[0].contains("operator attention"), "points the contributor at the operator path: {}", c[0]);

    // A later pass (admin retry that brought it back and failed again, or a stale re-check)
    // must not post a second comment.
    assert_eq!(relay_release(&e.d, &*e.signer, 4).await.unwrap(), RelayResult::Failed);
    assert_eq!(failure_comments(&e.notes).len(), 1, "not repeated on later passes");
}

/// #107: after exhausted retries (transient errors burned through MAX_ATTEMPTS) the FAILED
/// transition at the *last* attempt is the one that posts the comment. Earlier Transient
/// attempts must not have commented.
#[sqlx::test(migrator = "MIGRATOR")]
async fn exhausted_retries_tells_the_contributor_once_at_the_last_attempt(pool: PgPool) {
    let e = env(pool.clone()).await;
    queued(&e).await;
    e.notes.0.lock().unwrap().clear();

    for attempt in 1..MAX_ATTEMPTS {
        e.signer.submits.lock().unwrap().push_back(Err(ChainError::Busy));
        let r = relay_release(&e.d, &*e.signer, 4).await;
        assert!(matches!(r, Err(PayoutError::Transient(_))), "attempt {attempt}");
        assert!(failure_comments(&e.notes).is_empty(), "no comment during retries: attempt {attempt}");
    }
    e.signer.submits.lock().unwrap().push_back(Err(ChainError::Busy));
    assert_eq!(relay_release(&e.d, &*e.signer, 4).await.unwrap(), RelayResult::Failed);
    assert_eq!(failure_comments(&e.notes).len(), 1, "one comment on the final attempt");

    // A later pass is idempotent.
    assert_eq!(relay_release(&e.d, &*e.signer, 4).await.unwrap(), RelayResult::Failed);
    assert_eq!(failure_comments(&e.notes).len(), 1);
}

// ---- #109: the relayer classifies rejections by remediation ----

/// #109: a `TxBadSeq` rejection from `submit_release` must be transient. The relayer is
/// expected to re-read the sequence on the next attempt and try again.
#[sqlx::test(migrator = "MIGRATOR")]
async fn a_bad_sequence_rejection_is_retried_not_failed(pool: PgPool) {
    let e = env(pool.clone()).await;
    queued(&e).await;
    e.signer.submits.lock().unwrap().push_back(Err(ChainError::Rejected("TxBadSeq".into())));

    assert!(matches!(relay_release(&e.d, &*e.signer, 4).await, Err(PayoutError::Transient(_))));
    let p = payouts::for_bounty(&pool, 4).await.unwrap().unwrap();
    assert_eq!(p.status.as_str(), "QUEUED", "still retryable");
    assert_eq!(p.attempts, 1, "the attempt was recorded for backoff");
    assert!(failure_comments(&e.notes).is_empty(), "no failure comment while retrying");
}

/// #109: a `TxInsufficientFee` rejection is transient for the same reason - the next attempt
/// can bump the fee.
#[sqlx::test(migrator = "MIGRATOR")]
async fn an_insufficient_fee_rejection_is_retried_not_failed(pool: PgPool) {
    let e = env(pool.clone()).await;
    queued(&e).await;
    e.signer.submits.lock().unwrap().push_back(Err(ChainError::Rejected("TxInsufficientFee".into())));

    assert!(matches!(relay_release(&e.d, &*e.signer, 4).await, Err(PayoutError::Transient(_))));
    assert_eq!(status(&pool).await, "QUEUED");
}

/// #109: `TxInsufficientBalance` is permanent - nothing we can do without external top-up,
/// so the payout is failed on the first attempt without burning retries.
#[sqlx::test(migrator = "MIGRATOR")]
async fn an_insufficient_balance_rejection_fails_at_once(pool: PgPool) {
    let e = env(pool.clone()).await;
    queued(&e).await;
    e.notes.0.lock().unwrap().clear();
    e.signer.submits.lock().unwrap().push_back(Err(ChainError::Rejected("TxInsufficientBalance".into())));

    assert_eq!(relay_release(&e.d, &*e.signer, 4).await.unwrap(), RelayResult::Failed);
    let p = payouts::for_bounty(&pool, 4).await.unwrap().unwrap();
    assert_eq!((p.status.as_str(), p.attempts), ("FAILED", 1), "no retry budget burned");
    assert!(p.last_error.as_deref().unwrap_or("").contains("TxInsufficientBalance"));
    assert_eq!(failure_comments(&e.notes).len(), 1, "contributor is notified (#107)");
}

/// #109: a malformed / undecodable transaction result is permanent. The `describe_tx_error`
/// helper collapses these to the literal "rejected", which must also fail immediately.
#[sqlx::test(migrator = "MIGRATOR")]
async fn an_undecodable_rejection_fails_at_once(pool: PgPool) {
    let e = env(pool.clone()).await;
    queued(&e).await;
    e.signer.submits.lock().unwrap().push_back(Err(ChainError::Rejected("rejected".into())));

    assert_eq!(relay_release(&e.d, &*e.signer, 4).await.unwrap(), RelayResult::Failed);
    let p = payouts::for_bounty(&pool, 4).await.unwrap().unwrap();
    assert_eq!((p.status.as_str(), p.attempts), ("FAILED", 1));
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

/// #105: a full relayed release leaves an audit trail - PENDING_WALLET → QUEUED → SUBMITTED
/// → CONFIRMED - with the tx hash recorded on the submitted and confirmed rows.
#[sqlx::test(migrator = "MIGRATOR")]
async fn a_full_relayed_release_audits_each_status_transition(pool: PgPool) {
    use escrow_db::audit;
    let e = env(pool.clone()).await;
    queued(&e).await;
    assert_eq!(relay_release(&e.d, &*e.signer, 4).await.unwrap(), RelayResult::Confirmed);

    let payout_id = payouts::for_bounty(&pool, 4).await.unwrap().unwrap().id;
    let target = format!("payout:{payout_id}");
    // audit::recent is newest-first; the trail for this payout is QUEUED, SUBMITTED, CONFIRMED
    // (there is no PENDING_WALLET entry because the wallet was linked before the merge).
    let rows: Vec<_> = audit::recent(&pool, 100).await.unwrap().into_iter().filter(|r| r.target == target).collect();
    let actions: Vec<_> = rows.iter().rev().map(|r| r.action.as_str()).collect();
    assert_eq!(actions, ["payout.queued", "payout.submitted", "payout.confirmed"]);

    // The SUBMITTED entry must carry the tx hash; the CONFIRMED entry must carry it too.
    let submitted = rows.iter().find(|r| r.action == "payout.submitted").unwrap();
    let confirmed = rows.iter().find(|r| r.action == "payout.confirmed").unwrap();
    assert_eq!(submitted.data["txHash"], serde_json::Value::String("hash-1".into()));
    assert_eq!(submitted.data["from"], "QUEUED");
    assert_eq!(submitted.data["to"], "SUBMITTED");
    assert_eq!(confirmed.data["txHash"], serde_json::Value::String("hash-1".into()));
    assert_eq!(confirmed.data["from"], "SUBMITTED");
    assert_eq!(confirmed.data["to"], "CONFIRMED");
}

/// #105: a deterministic rejection records a FAILED audit entry with the error message
/// (no internal contract code leaks through say(); the audit trail is operator-facing).
#[sqlx::test(migrator = "MIGRATOR")]
async fn a_rejected_release_audits_the_failed_transition_with_the_error(pool: PgPool) {
    use escrow_db::audit;
    let e = env(pool.clone()).await;
    queued(&e).await;
    e.signer.submits.lock().unwrap().push_back(Err(ChainError::Simulation("HostError: Error(Contract, #17)".into())));
    assert_eq!(relay_release(&e.d, &*e.signer, 4).await.unwrap(), RelayResult::Failed);

    let payout_id = payouts::for_bounty(&pool, 4).await.unwrap().unwrap().id;
    let target = format!("payout:{payout_id}");
    let rows: Vec<_> = audit::recent(&pool, 100).await.unwrap().into_iter().filter(|r| r.target == target).collect();
    let failed = rows.iter().find(|r| r.action == "payout.failed").expect("payout.failed logged");
    assert_eq!(failed.data["from"], "QUEUED");
    assert_eq!(failed.data["to"], "FAILED");
    assert!(failed.data["error"].as_str().unwrap_or("").contains("#17"), "error is in the audit entry: {}", failed.data);
}

/// #102: a payout that the indexer confirmed while the row was backing off must not keep its
/// RetryTable entry alive forever. The next pass should prune ids whose payouts are no longer
/// returned by `payouts::due` (confirmed, refunded, deleted - anything).
#[sqlx::test(migrator = "MIGRATOR")]
async fn retry_table_entries_for_no_longer_due_payouts_are_pruned(pool: PgPool) {
    let e = env(pool.clone()).await;
    queued(&e).await;
    let mut retry = RetryTable::new();
    let t0 = Instant::now();

    // First pass: a transient failure parks the payout in the retry table.
    e.signer.submits.lock().unwrap().push_back(Err(ChainError::Busy));
    assert_eq!(relayer_pass(&e.d, &*e.signer, &mut retry, t0).await.unwrap(), 1);
    assert_eq!(retry.len(), 1, "the failed payout is backing off");
    let payout_id = payouts::for_bounty(&pool, 4).await.unwrap().unwrap().id;
    assert!(retry.contains_key(&payout_id));

    // Simulate the indexer confirming the payout from a release event while the row was
    // waiting: status moves to CONFIRMED, `payouts::due` no longer returns it.
    payouts::confirm_from_chain(&pool, 4, "e2e-tx", WINNER).await.unwrap();
    assert_eq!(status(&pool).await, "CONFIRMED");

    // The next pass must prune the stale entry even if the clock has not advanced past the
    // backoff window. With no due payouts there is no work to attempt, but the hygiene step
    // runs regardless of whether the backoff has elapsed.
    assert_eq!(
        relayer_pass(&e.d, &*e.signer, &mut retry, t0 + std::time::Duration::from_secs(1)).await.unwrap(),
        0,
        "nothing due"
    );
    assert!(retry.is_empty(), "stale entry pruned - was: {retry:?}");
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
    let paid = payouts::by_status(&pool, PayoutStatus::Confirmed, 10).await.unwrap();
    assert_eq!(paid.len(), 1, "the second was paid despite the first failing");
}
