//! Reviewing one PR against its bounty and posting (or editing) the single review comment.

use crate::{
    input::{ContextFile, FileChange, ReviewInput, build_user_prompt, context_candidates},
    review::{Ci, ModelError, ModelPort, Rankable, diff_hash, rank_prs, render_review, review_pr, sanitize_review},
    schema::{Flag, Verdict},
};
use chrono::{DateTime, Duration, Utc};
use escrow_db::{Db, bounties, github, models::Bounty, prs, reviews};
use escrow_github::{CiConclusion, GithubApi, GithubError, Issue};
use std::sync::Arc;
use thiserror::Error;

pub struct ReviewDeps {
    pub db: Db,
    pub gh: Arc<dyn GithubApi>,
    pub model: Arc<dyn ModelPort>,
    pub model_name: String,
    pub max_per_pr_per_day: i64,
    pub max_per_repo_per_day: i64,
    /// read a few repository files for review context (`AI_REPO_CONTEXT=basic`)
    pub repo_context: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReviewOutcome {
    Reviewed,
    /// this exact commit was already reviewed
    Cached,
    Draft,
    Closed,
    /// the per-PR or per-repository daily cap is reached
    RateLimited,
    NoBounty,
    NoPr,
    /// the repository has turned AI reviews off
    OptedOut,
}

#[derive(Debug, Error)]
pub enum ReviewError {
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    #[error("github: {0}")]
    Github(#[from] GithubError),
    #[error("model: {0}")]
    Model(#[from] ModelError),
}

impl ReviewError {
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Db(_) => true,
            Self::Github(e) => e.is_transient(),
            Self::Model(e) => e.is_transient(),
        }
    }
}

fn ci_str(c: CiConclusion) -> &'static str {
    match c {
        CiConclusion::Success => "success",
        CiConclusion::Failure => "failure",
        CiConclusion::Pending => "pending",
        CiConclusion::None => "none",
    }
}

fn ci_rank(c: CiConclusion) -> Ci {
    match c {
        CiConclusion::Success => Ci::Success,
        CiConclusion::Failure => Ci::Failure,
        CiConclusion::Pending => Ci::Pending,
        CiConclusion::None => Ci::None,
    }
}

fn verdict_of(s: &str) -> Verdict {
    serde_json::from_value(serde_json::Value::String(s.to_string())).unwrap_or_else(|_| {
        tracing::warn!(verdict = s, "unknown review verdict; treating it as needs_human");
        Verdict::NeedsHuman
    })
}

/// Merges the issues and criteria of every bounty a PR closes into one review input. A single bounty is passed
/// through unchanged; with several, each issue gets its own section and each criterion names its issue.
fn combine_bounties(live: &[Bounty], issues: Vec<Issue>) -> (String, String, Vec<String>) {
    if let ([b], [_]) = (live, issues.as_slice()) {
        let issue = issues.into_iter().next().expect("one issue");
        return (issue.title, issue.body.unwrap_or_default(), b.criteria.clone());
    }
    let title = issues.iter().map(|i| i.title.as_str()).collect::<Vec<_>>().join(" + ");
    let body = live
        .iter()
        .zip(&issues)
        .map(|(b, i)| format!("### Issue #{}: {}\n\n{}", b.issue_number, i.title, i.body.as_deref().unwrap_or_default()))
        .collect::<Vec<_>>()
        .join("\n\n");
    let criteria = live.iter().flat_map(|b| b.criteria.iter().map(move |c| format!("(#{}) {c}", b.issue_number))).collect();
    (title, body, criteria)
}

/// Reads a few repository files for review context beyond the diff. A failed read stops the reading and keeps what
/// was read, so a review never fails because context could not be fetched.
async fn gather_context(d: &ReviewDeps, inst: i64, name: &str, head: &str, changes: &[FileChange]) -> Vec<ContextFile> {
    if !d.repo_context {
        return vec![];
    }
    let mut out = vec![];
    for path in context_candidates(changes) {
        // CONTRIBUTING.md comes from the default branch; changed files are read at the PR's head commit
        let git_ref = if path == "CONTRIBUTING.md" { None } else { Some(head) };
        match d.gh.get_file_at(inst, name, git_ref, &path).await {
            Ok(Some(content)) => out.push(ContextFile { path, content }),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, "review context read stopped; reviewing with what was read");
                break;
            }
        }
    }
    out
}

/// Reviews one PR against every live bounty it closes. When a PR closes several bounties, their criteria are
/// combined into one review (one model call, one comment), and the review counts once per bounty against the
/// daily caps since it covers that much more work. Reviews are cached per commit, capped per PR and per repository
/// per day (cost control), and a re-review edits the earlier comment instead of adding another.
pub async fn run_pr_review(d: &ReviewDeps, now: DateTime<Utc>, pr_id: i64, force: bool) -> Result<ReviewOutcome, ReviewError> {
    let pool = &d.db.pool;
    let Some(pr) = prs::get(pool, pr_id).await? else {
        return Ok(ReviewOutcome::NoPr);
    };
    let live_bounties = prs::live_bounties(pool, pr_id).await?;
    let Some(bounty) = live_bounties.first() else {
        return Ok(ReviewOutcome::NoBounty);
    };
    let (_, repo) = bounties::with_repo(pool, bounty.id).await?.ok_or(sqlx::Error::RowNotFound)?;
    let (inst, name) = (repo.installation_id, repo.full_name.as_str());
    if !github::ai_reviews_enabled(pool, repo.id).await? {
        return Ok(ReviewOutcome::OptedOut);
    }

    let live = d.gh.get_pull(inst, name, pr.number).await?;
    if live.state != "open" {
        return Ok(ReviewOutcome::Closed);
    }
    if live.draft {
        return Ok(ReviewOutcome::Draft);
    }
    let sha = live.head.sha.clone();

    if !force && reviews::get(pool, pr_id, &sha).await?.is_some() {
        return Ok(ReviewOutcome::Cached);
    }
    // a combined review costs one unit per bounty it covers
    let cost = live_bounties.len() as i64;
    let since = now - Duration::days(1);
    if reviews::count_for_pr_since(pool, pr_id, since).await? + cost > d.max_per_pr_per_day
        || reviews::count_for_repo_since(pool, bounty.repo_id, since).await? + cost > d.max_per_repo_per_day
    {
        tracing::info!(pr = pr_id, "review skipped: daily cap");
        return Ok(ReviewOutcome::RateLimited);
    }

    let (files, ci) = tokio::join!(d.gh.get_pull_files(inst, name, pr.number), d.gh.get_ci_conclusion(inst, name, &sha));
    let files = files?;
    let mut issues = Vec::with_capacity(live_bounties.len());
    for b in &live_bounties {
        issues.push(d.gh.get_issue(inst, name, b.issue_number).await?);
    }
    let ci = ci.unwrap_or(CiConclusion::None);
    let changes: Vec<FileChange> = files
        .into_iter()
        .map(|f| FileChange { path: f.filename, additions: f.additions, deletions: f.deletions, patch: f.patch })
        .collect();
    let context = gather_context(d, inst, name, &sha, &changes).await;

    // signals computed by code, not by the model
    let hash = diff_hash(&changes);
    let mut extra = vec![];
    if !changes.is_empty() {
        for b in &live_bounties {
            if prs::has_duplicate_diff(pool, pr_id, b.id, &hash).await? {
                extra.push(Flag::PossibleDuplicate);
                break;
            }
        }
    }
    if ci == CiConclusion::Failure {
        extra.push(Flag::CiFailing);
    }

    let (issue_title, issue_body, criteria) = combine_bounties(&live_bounties, issues);
    let input = ReviewInput {
        issue_title,
        issue_body,
        criteria,
        pr_title: live.title.clone(),
        pr_body: live.body.clone().unwrap_or_default(),
        files: changes,
        ci: Some(ci_str(ci).to_string()),
        language: None,
        context,
    };
    let report = build_user_prompt(&input).context;
    let out = review_pr(d.model.as_ref(), &input).await?;
    let review = sanitize_review(out.review, &extra);

    // one comment per PR: edit the earlier one if we have it
    let previous = reviews::latest_commented(pool, pr_id).await?.and_then(|r| r.comment_id);
    let mut result = serde_json::to_value(&review).expect("review serializes");
    // record which context went in, so a maintainer can see what the reviewer could read
    result["context"] = serde_json::json!({ "included": report.included, "omitted": report.omitted });
    let flags: Vec<String> = review.flags.iter().map(|f| f.as_str().to_string()).collect();
    let saved = reviews::upsert(
        pool,
        &reviews::NewReview {
            pr_id,
            head_sha: &sha,
            model: &d.model_name,
            verdict: review.verdict.as_str(),
            score: review.score,
            result: &result,
            flags: &flags,
            input_tokens: out.input_tokens as i32,
            output_tokens: out.output_tokens as i32,
            comment_id: previous,
        },
    )
    .await?;
    prs::set_diff(pool, pr_id, &hash, &sha).await?;

    // rank among this bounty's open PRs by their newest review (PR ids grow with time, so they stand in for submission order)
    let rankables: Vec<Rankable> = prs::open_with_latest_review(pool, bounty.id)
        .await?
        .into_iter()
        .filter_map(|(p, r)| {
            r.map(|r| Rankable {
                id: p.id,
                verdict: verdict_of(&r.verdict),
                score: r.score,
                ci: if p.id == pr_id { ci_rank(ci) } else { Ci::None },
                submitted_at: p.id,
            })
        })
        .collect();
    let order = rank_prs(&rankables);
    let rank = (order.iter().position(|id| *id == pr_id).map_or(1, |i| i + 1), order.len().max(1));
    let body = render_review(&review, Some(rank), &sha, &d.model_name);

    match saved.comment_id {
        Some(cid) => d.gh.update_comment(inst, name, cid, &body).await?,
        None => {
            let cid = d.gh.post_comment(inst, name, pr.number, &body).await?;
            reviews::set_comment(pool, &saved.id, cid).await?;
        }
    }
    Ok(ReviewOutcome::Reviewed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_verdicts_parse_and_unknown_ones_fall_back_to_needs_human() {
        assert_eq!(verdict_of("likely_meets"), Verdict::LikelyMeets);
        assert_eq!(verdict_of("does_not_meet"), Verdict::DoesNotMeet);
        assert_eq!(verdict_of("totally_new_verdict"), Verdict::NeedsHuman);
    }
}
