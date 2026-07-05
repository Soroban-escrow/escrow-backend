use crate::{
    payout::{PayoutDeps, relay_release},
    ports::PayoutError,
};
use escrow_chain::ReleaseSigner;
use escrow_db::payouts;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

/// When each payout may next be tried (it failed transiently and is backing off).
pub type RetryTable = HashMap<String, Instant>;

/// Exponential backoff between attempts of the same payout: 2 s, 4 s, 8 s, ... capped at 5 minutes.
pub fn backoff(attempts: i32) -> Duration {
    Duration::from_secs((1u64 << attempts.clamp(1, 20) as u32).min(300))
}

/// One pass: queued and submitted payouts one at a time (a single signing account, so sequence numbers
/// never race). Returns how many were attempted.
pub async fn relayer_pass(
    d: &PayoutDeps,
    signer: &dyn ReleaseSigner,
    retry_at: &mut RetryTable,
    now: Instant,
) -> Result<usize, PayoutError> {
    let due = payouts::due(&d.db.pool, 20).await?;
    let mut attempted = 0;
    for p in due {
        if retry_at.get(&p.id).is_some_and(|t| *t > now) {
            continue;
        }
        attempted += 1;
        match relay_release(d, signer, p.bounty_id).await {
            Ok(result) => {
                retry_at.remove(&p.id);
                tracing::info!(bounty = p.bounty_id, ?result, "relayer");
            }
            Err(e) => {
                retry_at.insert(p.id.clone(), now + backoff(p.attempts + 1));
                tracing::warn!(bounty = p.bounty_id, error = %e, "relayer transient failure");
            }
        }
    }
    Ok(attempted)
}

pub async fn run_relayer(d: PayoutDeps, signer: &dyn ReleaseSigner, stop: CancellationToken, interval: Duration) {
    let mut retry_at = RetryTable::new();
    while !stop.is_cancelled() {
        if let Err(e) = relayer_pass(&d, signer, &mut retry_at, Instant::now()).await {
            tracing::error!(error = %e, "relayer pass failed");
        }
        tokio::select! { _ = stop.cancelled() => break, _ = tokio::time::sleep(interval) => {} }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff(1), Duration::from_secs(2));
        assert_eq!(backoff(2), Duration::from_secs(4));
        assert_eq!(backoff(3), Duration::from_secs(8));
        assert_eq!(backoff(0), Duration::from_secs(2));
        assert_eq!(backoff(9), Duration::from_secs(300));
        assert_eq!(backoff(1000), Duration::from_secs(300));
        assert_eq!(backoff(-5), Duration::from_secs(2));
    }
}
