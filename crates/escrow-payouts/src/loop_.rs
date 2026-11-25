use crate::{
    payout::{PayoutDeps, relay_release},
    ports::PayoutError,
};
use escrow_chain::ReleaseSigner;
use escrow_db::payouts;
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    task::Poll,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

/// When each payout may next be tried (it failed transiently and is backing off).
pub type RetryTable = HashMap<String, Instant>;

/// Exponential backoff between attempts of the same payout: 2 s, 4 s, 8 s, ... capped at 5 minutes.
pub fn backoff(attempts: i32) -> Duration {
    Duration::from_secs((1u64 << attempts.clamp(1, 20) as u32).min(300))
}

/// Polls every future in this task, so one pass can run several releases at once without spawning (the futures
/// borrow the pass's dependencies). Results come back in input order.
async fn join_all_local<F: Future>(futs: Vec<F>) -> Vec<F::Output> {
    let mut pending: Vec<Option<Pin<Box<F>>>> = futs.into_iter().map(|f| Some(Box::pin(f))).collect();
    let mut outs: Vec<Option<F::Output>> = pending.iter().map(|_| None).collect();
    std::future::poll_fn(move |cx| {
        let mut waiting = false;
        for (slot, out) in pending.iter_mut().zip(outs.iter_mut()) {
            if let Some(f) = slot {
                match f.as_mut().poll(cx) {
                    Poll::Ready(v) => {
                        *out = Some(v);
                        *slot = None;
                    }
                    Poll::Pending => waiting = true,
                }
            }
        }
        if waiting {
            Poll::Pending
        } else {
            Poll::Ready(outs.iter_mut().map(|o| o.take().expect("every release finished")).collect())
        }
    })
    .await
}

/// One pass over queued and submitted payouts. Releases run `signer.max_parallel()` at a time: one per channel
/// account, or one at a time without channels. Returns how many were attempted.
pub async fn relayer_pass(
    d: &PayoutDeps,
    signer: &dyn ReleaseSigner,
    retry_at: &mut RetryTable,
    now: Instant,
) -> Result<usize, PayoutError> {
    let due = payouts::due(&d.db.pool, 20).await?;
    let ready: Vec<_> = due.into_iter().filter(|p| !retry_at.get(&p.id).is_some_and(|t| *t > now)).collect();
    let width = signer.max_parallel().max(1);
    let mut attempted = 0;
    for chunk in ready.chunks(width) {
        attempted += chunk.len();
        let results = join_all_local(chunk.iter().map(|p| relay_release(d, signer, p.bounty_id)).collect()).await;
        for (p, result) in chunk.iter().zip(results) {
            match result {
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

    #[tokio::test]
    async fn join_all_local_keeps_input_order_and_finishes_every_future() {
        let futs: Vec<_> = (0..5u32).map(|i| async move { i * 10 }).collect();
        assert_eq!(join_all_local(futs).await, vec![0, 10, 20, 30, 40]);
    }
}
