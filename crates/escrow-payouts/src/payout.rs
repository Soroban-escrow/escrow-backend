use crate::ports::{Notifier, PayoutError};
use escrow_chain::{ChainReader, ReleaseSigner, TxOutcome};
use escrow_core::{BountyStatus, PayoutStatus};
use escrow_db::{Db, audit, payouts, users};
use serde_json::json;
use std::sync::Arc;

pub const MAX_ATTEMPTS: i32 = 5;
/// A submitted transaction expires after the relayer's 2-minute validity window; after this long it provably cannot land.
const STALE_SUBMISSION_SECS: i64 = 5 * 60;

#[derive(Clone)]
pub struct PayoutDeps {
    pub db: Db,
    pub chain: Arc<dyn ChainReader>,
    pub notifier: Arc<dyn Notifier>,
    pub web_url: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum MergeOutcome {
    Created,
    /// another merged PR already claimed this bounty
    Duplicate,
    /// the chain says the bounty is no longer open (already paid, refunded, disputed...)
    NotOpen,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Advance {
    NoPayout,
    AlreadyMoving(String),
    NotOpen,
    PendingWallet,
    Failed,
    Queued,
    AwaitingMaintainer,
    /// someone else moved it first
    Raced,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RelayResult {
    Confirmed,
    Failed,
}

fn id_u64(id: i64) -> u64 {
    u64::try_from(id).unwrap_or(0)
}

async fn say(d: &PayoutDeps, bounty: i64, pr: i32, msg: &str) {
    if let Err(e) = d.notifier.notify(bounty, pr, msg).await {
        tracing::warn!(bounty, error = %e, "payout comment failed (best effort)");
    }
}

/// #105: records a payout status transition in the audit log. Best-effort - an audit write
/// failure is logged and does not propagate, so a payout never fails because the audit trail
/// is unavailable. Called adjacent to each `payouts::mark_*` in this module; the indexer's
/// confirm-from-chain path audits inside the same database transaction as the UPDATE.
pub(crate) async fn audit_payout_transition(
    d: &PayoutDeps,
    payout_id: &str,
    from_status: &str,
    to_status: &str,
    tx_hash: Option<&str>,
    error: Option<&str>,
) {
    let data = json!({
        "from": from_status,
        "to": to_status,
        "txHash": tx_hash,
        "error": error,
    });
    if let Err(e) =
        audit::log(&d.db.pool, "payout", &format!("payout.{}", to_status.to_ascii_lowercase()), &format!("payout:{payout_id}"), &data)
            .await
    {
        tracing::warn!(payout = %payout_id, error = %e, "payout transition audit write failed (best effort)");
    }
}

/// #107: comment the contributor sees on the PR after a FAILED transition. It never names the
/// internal cause (a `ChainError` may carry the RPC URL in an error_result_xdr tail, a contract
/// code gives away which guard tripped) and points at the operator path. Posted once per payout:
/// `payouts::claim_failure_notice` guarantees exactly one claim across concurrent callers.
const FAILURE_COMMENT: &str =
    "This payout could not be completed and has been flagged for operator attention. No action is needed from you.";

async fn notify_once_on_fail(d: &PayoutDeps, payout_id: &str, bounty: i64, pr: i32) {
    match payouts::claim_failure_notice(&d.db.pool, payout_id).await {
        Ok(true) => say(d, bounty, pr, FAILURE_COMMENT).await,
        Ok(false) => {}
        Err(e) => tracing::warn!(payout = %payout_id, error = %e, "claim_failure_notice failed (best effort)"),
    }
}

/// A linked PR was merged. First merge wins: one payout per bounty. Re-reads the bounty from the chain first.
pub async fn on_merge(d: &PayoutDeps, bounty_id: i64, pr_number: i32, author_github_id: i64) -> Result<MergeOutcome, PayoutError> {
    if d.chain.get_bounty(id_u64(bounty_id)).await?.status != BountyStatus::Open {
        return Ok(MergeOutcome::NotOpen);
    }
    if payouts::create_pending(&d.db.pool, bounty_id, pr_number, author_github_id).await?.is_none() {
        say(d, bounty_id, pr_number, "Another merged PR already claimed this bounty; no second payout is made.").await;
        return Ok(MergeOutcome::Duplicate);
    }
    advance(d, bounty_id).await?;
    Ok(MergeOutcome::Created)
}

/// Moves a PENDING_WALLET payout forward. Also used when a wallet is linked late.
pub async fn advance(d: &PayoutDeps, bounty_id: i64) -> Result<Advance, PayoutError> {
    let Some(p) = payouts::for_bounty(&d.db.pool, bounty_id).await? else {
        return Ok(Advance::NoPayout);
    };
    if p.status() != PayoutStatus::PendingWallet {
        return Ok(Advance::AlreadyMoving(p.status));
    }
    let chain = d.chain.get_bounty(id_u64(bounty_id)).await?;
    if chain.status != BountyStatus::Open {
        return Ok(Advance::NotOpen);
    }
    let wallet = users::by_github_id(&d.db.pool, p.winner_github_id).await?.and_then(|u| u.wallet);
    let Some(wallet) = wallet else {
        say(d, bounty_id, p.pr_number, &format!("Link your wallet at {}/me to receive your payout.", d.web_url)).await;
        return Ok(Advance::PendingWallet);
    };
    if wallet == chain.maintainer {
        payouts::mark_failed(&d.db.pool, &p.id, "winner is the maintainer", false).await?;
        audit_payout_transition(d, &p.id, &p.status, "FAILED", None, Some("winner is the maintainer")).await;
        notify_once_on_fail(d, &p.id, bounty_id, p.pr_number).await;
        return Ok(Advance::Failed);
    }
    let next = if chain.relayer_allowed { PayoutStatus::Queued } else { PayoutStatus::AwaitingMaintainer };
    // guarded update: of several concurrent callers exactly one moves it out of PENDING_WALLET
    if !payouts::release_from_pending(&d.db.pool, &p.id, next, &wallet).await? {
        return Ok(Advance::Raced);
    }
    audit_payout_transition(d, &p.id, &p.status, next.as_str(), None, None).await;
    if next == PayoutStatus::AwaitingMaintainer {
        say(d, bounty_id, p.pr_number, &format!("The maintainer must release this bounty at {}/b/{bounty_id}.", d.web_url)).await;
        return Ok(Advance::AwaitingMaintainer);
    }
    Ok(Advance::Queued)
}

/// A wallet was linked: continue every payout that was waiting for it. Returns how many were waiting.
pub async fn on_wallet_linked(d: &PayoutDeps, github_id: i64) -> Result<usize, PayoutError> {
    let pending = payouts::pending_for_winner(&d.db.pool, github_id).await?;
    for p in &pending {
        advance(d, p.bounty_id).await?;
    }
    Ok(pending.len())
}

/// One relayer attempt for a bounty. `Err(Transient)` asks the caller to back off and retry;
/// deterministic failures (the contract said no) and exhausted retries end as `Ok(Failed)`.
pub async fn relay_release(d: &PayoutDeps, signer: &dyn ReleaseSigner, bounty_id: i64) -> Result<RelayResult, PayoutError> {
    let pool = &d.db.pool;
    let Some(p) = payouts::for_bounty(pool, bounty_id).await? else {
        return Err(PayoutError::Other(format!("no payout for bounty {bounty_id}")));
    };
    match p.status() {
        PayoutStatus::Confirmed => return Ok(RelayResult::Confirmed),
        PayoutStatus::Failed => return Ok(RelayResult::Failed),
        _ => {}
    }
    let Some(winner) = p.winner_address.clone() else {
        return Err(PayoutError::Other("payout has no winner address".into()));
    };

    // A hash we already submitted: check it instead of submitting again.
    if p.status() == PayoutStatus::Submitted
        && let Some(hash) = &p.tx_hash
    {
        return settle(d, signer, &p, &winner, hash).await;
    }

    // Our earlier transaction landed but we crashed before recording it: the chain is the truth.
    let chain = d.chain.get_bounty(id_u64(bounty_id)).await?;
    if chain.status == BountyStatus::Released && chain.winner.as_deref() == Some(winner.as_str()) {
        payouts::mark_confirmed(pool, &p.id, None).await?;
        audit_payout_transition(d, &p.id, &p.status, "CONFIRMED", p.tx_hash.as_deref(), None).await;
        return Ok(RelayResult::Confirmed);
    }
    if chain.status != BountyStatus::Open || !chain.relayer_allowed {
        let why = format!("bounty is {}, relayerAllowed={}", chain.status, chain.relayer_allowed);
        payouts::mark_failed(pool, &p.id, &why, false).await?;
        audit_payout_transition(d, &p.id, &p.status, "FAILED", None, Some(&why)).await;
        notify_once_on_fail(d, &p.id, bounty_id, p.pr_number).await;
        return Ok(RelayResult::Failed);
    }

    let hash = match signer.submit_release(id_u64(bounty_id), &winner).await {
        Ok(h) => h,
        // #109: a rejected transaction result (TxBadSeq, TxInsufficientFee) should retry; a
        // contract rejection or a non-retryable Tx result (TxInsufficientBalance, malformed)
        // is deterministic and must fail immediately.
        Err(e) => return fail(d, &p, e.is_permanent_rejection(), e.to_string()).await,
    };
    // Record the hash BEFORE waiting, so a crash never leads to a second submission.
    payouts::mark_submitted(pool, &p.id, &hash).await?;
    audit_payout_transition(d, &p.id, &p.status, "SUBMITTED", Some(&hash), None).await;
    let p = payouts::get(pool, &p.id).await?.unwrap_or(p);
    settle(d, signer, &p, &winner, &hash).await
}

async fn settle(
    d: &PayoutDeps,
    signer: &dyn ReleaseSigner,
    p: &escrow_db::Payout,
    winner: &str,
    hash: &str,
) -> Result<RelayResult, PayoutError> {
    let pool = &d.db.pool;
    match signer.wait_for_tx(hash).await {
        Ok(TxOutcome::Success) => {
            // The "Paid X to @login" comment is posted when the indexer sees the release event (covers every release path).
            payouts::mark_confirmed(pool, &p.id, Some(hash)).await?;
            audit_payout_transition(d, &p.id, &p.status, "CONFIRMED", Some(hash), None).await;
            Ok(RelayResult::Confirmed)
        }
        Ok(TxOutcome::Failed) => {
            payouts::mark_failed(pool, &p.id, "transaction failed on chain", false).await?;
            audit_payout_transition(d, &p.id, &p.status, "FAILED", p.tx_hash.as_deref(), Some("transaction failed on chain")).await;
            notify_once_on_fail(d, &p.id, p.bounty_id, p.pr_number).await;
            Ok(RelayResult::Failed)
        }
        Ok(TxOutcome::NotFound) => {
            let age = (chrono::Utc::now() - p.updated_at).num_seconds();
            if age < STALE_SUBMISSION_SECS {
                return fail(d, p, false, format!("transaction {hash} not seen yet")).await;
            }
            // Too old to ever land (it expired). The chain says whether we were paid anyway.
            let chain = d.chain.get_bounty(id_u64(p.bounty_id)).await?;
            if chain.status == BountyStatus::Released && chain.winner.as_deref() == Some(winner) {
                payouts::mark_confirmed(pool, &p.id, None).await?;
                audit_payout_transition(d, &p.id, &p.status, "CONFIRMED", p.tx_hash.as_deref(), None).await;
                return Ok(RelayResult::Confirmed);
            }
            if chain.status == BountyStatus::Open {
                payouts::requeue(pool, &p.id).await?;
                audit_payout_transition(d, &p.id, &p.status, "QUEUED", p.tx_hash.as_deref(), Some("submitted transaction expired; resubmitting")).await;
                return Err(PayoutError::Transient("submitted transaction expired; resubmitting".into()));
            }
            let why = format!("bounty became {} while waiting", chain.status);
            payouts::mark_failed(pool, &p.id, &why, false).await?;
            audit_payout_transition(d, &p.id, &p.status, "FAILED", p.tx_hash.as_deref(), Some(&why)).await;
            notify_once_on_fail(d, &p.id, p.bounty_id, p.pr_number).await;
            Ok(RelayResult::Failed)
        }
        // #109: `wait_for_tx` network-level errors are transient; a permanent rejection surfaced
        // here (contract error on a getTransaction poll is unusual, but is_permanent_rejection
        // covers it) is deterministic.
        Err(e) => fail(d, p, e.is_permanent_rejection(), e.to_string()).await,
    }
}

/// Deterministic rejections and exhausted retries end the payout as FAILED; anything else is retried.
async fn fail(d: &PayoutDeps, p: &escrow_db::Payout, deterministic: bool, msg: String) -> Result<RelayResult, PayoutError> {
    if deterministic || p.attempts + 1 >= MAX_ATTEMPTS {
        payouts::mark_failed(&d.db.pool, &p.id, &msg, true).await?;
        audit_payout_transition(d, &p.id, &p.status, "FAILED", p.tx_hash.as_deref(), Some(&msg)).await;
        notify_once_on_fail(d, &p.id, p.bounty_id, p.pr_number).await;
        return Ok(RelayResult::Failed);
    }
    payouts::record_attempt_error(&d.db.pool, &p.id, &msg).await?;
    Err(PayoutError::Transient(msg))
}
