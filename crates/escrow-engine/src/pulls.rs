//! Pull requests that close bountied issues: track them, trigger reviews, and start the payout on merge.

use crate::ports::ReviewScheduler;
use escrow_core::refs::parse_closing_refs;
use escrow_db::{Db, bounties, github, prs};
use escrow_github::payloads::PullRequestEvent;
use escrow_payouts::{PayoutDeps, on_merge};
use std::{sync::Arc, time::Duration};

#[derive(Clone)]
pub struct PullDeps {
    pub db: Db,
    pub payouts: PayoutDeps,
    pub reviews: Option<Arc<dyn ReviewScheduler>>,
    /// quiet period before a review starts, so a burst of pushes costs one review
    pub review_delay: Duration,
}

const REVIEW_ACTIONS: [&str; 5] = ["opened", "synchronize", "reopened", "ready_for_review", "edited"];

/// Returns the bounty ids the PR is linked to. Never lets the maintainer's own PRs earn their bounty
/// (the contract blocks that too), and never reopens the record of a merged PR.
pub async fn handle_pull_request(d: &PullDeps, ev: &PullRequestEvent) -> Result<Vec<i64>, crate::EngineError> {
    let pr = &ev.pull_request;
    if pr.user.kind == "Bot" {
        return Ok(vec![]);
    }
    // Once merged the record is frozen: never relink, unlink or re-review it.
    if prs::get(&d.db.pool, pr.id).await?.is_some_and(|p| p.state == "merged") {
        return Ok(vec![]);
    }
    let repo_id = ev.repository.id;
    let issues: Vec<i32> =
        parse_closing_refs(pr.body.as_deref(), &repo_name(ev)).into_iter().filter_map(|n| i32::try_from(n).ok()).collect();

    // Drop links whose closing reference is gone. Only while the PR is unmerged, and only on the events
    // that can change what it closes (an edit to the body, or a synchronize that may rewrite it). A merged
    // PR's links are frozen (handled above); other actions never remove a still-pending submission. Only
    // live bounties are considered, so a link to an already-resolved bounty is left as history.
    if !pr.merged && matches!(ev.action.as_str(), "edited" | "synchronize") {
        let referenced: std::collections::HashSet<i32> = issues.iter().copied().collect();
        for b in prs::live_bounties(&d.db.pool, pr.id).await? {
            if b.repo_id == repo_id && !referenced.contains(&b.issue_number) {
                prs::unlink(&d.db.pool, pr.id, b.id).await?;
            }
        }
    }

    let live = bounties::active_for_issues(&d.db.pool, repo_id, &issues).await?;
    let eligible: Vec<_> = live.into_iter().filter(|b| b.maintainer_github_id != Some(pr.user.id)).collect();
    if eligible.is_empty() {
        return Ok(vec![]);
    }
    let state = if pr.merged {
        "merged"
    } else if pr.state == "closed" {
        "closed"
    } else {
        "open"
    };
    prs::upsert(
        &d.db.pool,
        &prs::PrUpsert {
            id: pr.id,
            number: pr.number,
            author_github_id: pr.user.id,
            author_login: &pr.user.login,
            head_sha: &pr.head.sha,
            state,
            merged_at: pr.merged_at,
        },
    )
    .await?;
    // one PR can close several bountied issues: one link per (PR, bounty)
    for b in &eligible {
        prs::link(&d.db.pool, pr.id, b.id).await?;
        if pr.merged && ev.action == "closed" {
            on_merge(&d.payouts, b.id, pr.number, pr.user.id).await?;
        }
    }
    if !pr.merged
        && state == "open"
        && !pr.draft
        && REVIEW_ACTIONS.contains(&ev.action.as_str())
        && (ev.action != "edited" || ev.changes.as_ref().is_some_and(|c| c.affects_review()))
        && let Some(r) = &d.reviews
        && github::ai_reviews_enabled(&d.db.pool, ev.repository.id).await?
    {
        // best effort: a scheduling hiccup must not fail the webhook
        if let Err(e) = r.schedule(pr.id, d.review_delay, false).await {
            tracing::warn!(pr = pr.id, error = %e, "could not schedule a review");
        }
    }
    Ok(eligible.iter().map(|b| b.id).collect())
}

fn repo_name(ev: &PullRequestEvent) -> String {
    ev.repository.full_name.clone()
}
