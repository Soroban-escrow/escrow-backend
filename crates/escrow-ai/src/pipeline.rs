//! Reviewing one PR against its bounty and posting (or editing) the single review comment.

use crate::{
    input::{FileChange, ReviewInput},
    review::{Ci, ModelError, ModelPort, Rankable, diff_hash, rank_prs, render_review, review_pr, sanitize_review},
    schema::{Flag, Verdict},
};
use chrono::{DateTime, Duration, Utc};
use escrow_db::{Db, bounties, prs, reviews};
use escrow_github::{CiConclusion, GithubApi, GithubError};
use std::sync::Arc;
use thiserror::Error;

pub struct ReviewDeps {
    pub db: Db,
    pub gh: Arc<dyn GithubApi>,
    pub model: Arc<dyn ModelPort>,
    pub model_name: String,
    pub max_per_pr_per_day: i64,
    pub max_per_repo_per_day: i64,
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
    serde_json::from_value(serde_json::Value::String(s.to_string())).unwrap_or(Verdict::NeedsHuman)
}

/// Reviews one PR against the first live bounty it closes. Reviews are cached per commit, capped per PR and per
/// repository per day (cost control), and a re-review edits the earlier comment instead of adding another.
pub async fn run_pr_review(d: &ReviewDeps, now: DateTime<Utc>, pr_id: i64, force: bool) -> Result<ReviewOutcome, ReviewError> {
    let pool = &d.db.pool;
    let Some(pr) = prs::get(pool, pr_id).await? else {
        return Ok(ReviewOutcome::NoPr);
    };
    let Some(bounty) = prs::live_bounties(pool, pr_id).await?.into_iter().next() else {
        return Ok(ReviewOutcome::NoBounty);
    };
    let (_, repo) = bounties::with_repo(pool, bounty.id).await?.ok_or(sqlx::Error::RowNotFound)?;
    let (inst, name) = (repo.installation_id, repo.full_name.as_str());

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
    let since = now - Duration::days(1);
    if reviews::count_for_pr_since(pool, pr_id, since).await? >= d.max_per_pr_per_day
        || reviews::count_for_repo_since(pool, bounty.repo_id, since).await? >= d.max_per_repo_per_day
    {
        tracing::info!(pr = pr_id, "review skipped: daily cap");
        return Ok(ReviewOutcome::RateLimited);
    }

    let (issue, files, ci, readme) = tokio::join!(
        d.gh.get_issue(inst, name, bounty.issue_number),
        d.gh.get_pull_files(inst, name, pr.number),
        d.gh.get_ci_conclusion(inst, name, &sha),
        d.gh.get_readme_excerpt(inst, name, 2000),
    );
    let (issue, files) = (issue?, files?);
    let ci = ci.unwrap_or(CiConclusion::None);
    let _readme = readme.unwrap_or_default();
    let changes: Vec<FileChange> = files
        .into_iter()
        .map(|f| FileChange { path: f.filename, additions: f.additions, deletions: f.deletions, patch: f.patch })
        .collect();

    // signals computed by code, not by the model
    let hash = diff_hash(&changes);
    let mut extra = vec![];
    if !changes.is_empty() && prs::has_duplicate_diff(pool, pr_id, bounty.id, &hash).await? {
        extra.push(Flag::PossibleDuplicate);
    }
    if ci == CiConclusion::Failure {
        extra.push(Flag::CiFailing);
    }

    let out = review_pr(
        d.model.as_ref(),
        &ReviewInput {
            issue_title: issue.title,
            issue_body: issue.body.unwrap_or_default(),
            criteria: bounty.criteria.clone(),
            pr_title: live.title.clone(),
            pr_body: live.body.clone().unwrap_or_default(),
            files: changes,
            ci: Some(ci_str(ci).to_string()),
            language: None,
        },
    )
    .await?;
    let review = sanitize_review(out.review, &extra);

    // one comment per PR: edit the earlier one if we have it
    let previous = reviews::latest_commented(pool, pr_id).await?.and_then(|r| r.comment_id);
    let result = serde_json::to_value(&review).expect("review serializes");
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
