//! The `/bounty` comment commands.

use crate::{error::EngineError, ports::ReviewScheduler};
use chrono::{DateTime, Duration, Utc};
use escrow_core::{
    amounts::display,
    command::{Command, TokenSymbol, parse_command},
    security::random_b64url,
};
use escrow_db::{Db, audit, bounties, disputes, intents, prs};
use escrow_github::{GithubApi, payloads::IssueCommentEvent};
use std::sync::Arc;

/// Contract ids of the tokens bounties can be paid in, and the per-bounty limits (base units).
#[derive(Clone)]
pub struct TokenTable {
    pub usdc: Option<String>,
    pub xlm: Option<String>,
    pub min: i128,
    pub max: i128,
}

impl TokenTable {
    fn address(&self, t: TokenSymbol) -> Option<&str> {
        match t {
            TokenSymbol::Usdc => self.usdc.as_deref().filter(|a| !a.is_empty()),
            TokenSymbol::Xlm => self.xlm.as_deref().filter(|a| !a.is_empty()),
        }
    }
}

#[derive(Clone)]
pub struct CommandDeps {
    pub db: Db,
    pub gh: Arc<dyn GithubApi>,
    pub web_url: String,
    pub tokens: TokenTable,
    pub min_days: u32,
    pub max_days: u32,
    /// where `/bounty review` sends its request (absent when AI review is off)
    pub reviews: Option<Arc<dyn ReviewScheduler>>,
    pub now: fn() -> DateTime<Utc>,
}

const WRITERS: [&str; 3] = ["admin", "maintain", "write"];
const HELP: &str = "**SkillEscrow commands**\n\
- `/bounty <amount> [USDC|XLM] [<days>d]` fund this issue, e.g. `/bounty 50 USDC 14d` (needs write access)\n\
- `/bounty dispute <reason>` flag a problem on a bountied issue\n\
- `/bounty help`";

/// What happened, as a short label for logs and tests. User mistakes get a friendly reply and are not errors;
/// only infrastructure failures are.
pub async fn handle_issue_comment(d: &CommandDeps, ev: &IssueCommentEvent) -> Result<String, EngineError> {
    if ev.comment.user.kind == "Bot" {
        return Ok("ignored:bot".into());
    }
    let Some(cmd) = parse_command(&ev.comment.body) else {
        return Ok("ignored:not-a-command".into());
    };
    let Some(inst) = ev.installation.as_ref().map(|i| i.id) else {
        return Ok("ignored:no-installation".into());
    };
    let repo = ev.repository.full_name.as_str();
    let number = ev.issue.number;
    let reply = |body: String| async move { d.gh.post_comment(inst, repo, number, &body).await };
    let login = ev.comment.user.login.as_str();

    match cmd {
        Command::Help => {
            reply(HELP.into()).await?;
            Ok("help".into())
        }
        Command::Error(m) => {
            reply(format!("{m}\n\n{HELP}")).await?;
            Ok("error".into())
        }
        Command::Review => {
            if ev.issue.pull_request.is_none() {
                return Ok("ignored:review-not-on-issues".into());
            }
            if !WRITERS.contains(&d.gh.get_permission(inst, repo, login).await?.as_str()) {
                reply(format!("@{login} only collaborators with write access can request a review.")).await?;
                return Ok("denied".into());
            }
            let Some(pr) = prs::find_linked_by_number(&d.db.pool, ev.repository.id, number).await? else {
                reply("This pull request is not linked to a bounty, so there is nothing to review against.".into()).await?;
                return Ok("rejected:not-linked".into());
            };
            let Some(reviews) = &d.reviews else {
                return Ok("ignored:reviews-disabled".into());
            };
            reviews.schedule(pr.id, std::time::Duration::ZERO, true).await.map_err(EngineError::Other)?;
            reply("Re-review queued. The review comment on this PR will be updated shortly.".into()).await?;
            Ok("review-requested".into())
        }
        Command::Dispute { reason } => {
            let Some(b) = bounties::latest_for_issue(&d.db.pool, ev.repository.id, number).await? else {
                reply("There is no bounty on this issue.".into()).await?;
                return Ok("rejected:no-bounty".into());
            };
            let uid = ev.comment.user.id;
            let pr_author = prs::for_bounty(&d.db.pool, b.id).await?.iter().any(|p| p.author_github_id == uid);
            if b.maintainer_github_id != Some(uid) && !pr_author {
                reply("Only the maintainer or an author of a linked PR can open a dispute.".into()).await?;
                return Ok("denied".into());
            }
            let reason: String = reason.chars().take(2000).collect();
            let dsp = disputes::create(&d.db.pool, b.id, uid, &reason).await?;
            audit::log(
                &d.db.pool,
                &format!("github:{uid}"),
                "dispute.requested",
                &format!("bounty:{}", b.id),
                &serde_json::json!({ "dispute": dsp.id }),
            )
            .await?;
            reply("Dispute recorded. A human will review this within 48 hours.".into()).await?;
            Ok("dispute-requested".into())
        }
        Command::Fund { amount, token, days } => {
            if !WRITERS.contains(&d.gh.get_permission(inst, repo, login).await?.as_str()) {
                reply(format!("@{login} only collaborators with write access can fund a bounty.")).await?;
                return Ok("denied".into());
            }
            if ev.issue.pull_request.is_some() {
                reply("Bounties can only be placed on issues, not pull requests.".into()).await?;
                return Ok("rejected:pr".into());
            }
            if ev.issue.state != "open" {
                reply("This issue is closed.".into()).await?;
                return Ok("rejected:closed".into());
            }
            if bounties::find_active_for_issue(&d.db.pool, ev.repository.id, number).await?.is_some() {
                reply("This issue already has an active bounty.".into()).await?;
                return Ok("rejected:active".into());
            }
            let Some(address) = d.tokens.address(token) else {
                reply(format!("{} bounties are not enabled on this deployment.", token.as_str())).await?;
                return Ok("rejected:token".into());
            };
            if amount < d.tokens.min || amount > d.tokens.max {
                reply(format!("Amount must be between {} and {} {}.", display(d.tokens.min, 0), display(d.tokens.max, 0), token.as_str()))
                    .await?;
                return Ok("rejected:amount".into());
            }
            if days < d.min_days || days > d.max_days {
                reply(format!("Duration must be between {} and {} days.", d.min_days, d.max_days)).await?;
                return Ok("rejected:duration".into());
            }

            // Store the link first, then announce it, so a link in a comment always exists.
            let id = random_b64url::<24>();
            let expires_at = (d.now)() + Duration::hours(24);
            intents::create(
                &d.db.pool,
                &intents::NewIntent {
                    id: &id,
                    repo_id: ev.repository.id,
                    issue_number: number,
                    requested_by: ev.comment.user.id,
                    token: address,
                    amount,
                    duration_days: days as i32,
                    comment_id: 0,
                    expires_at,
                },
            )
            .await?;
            let link = format!("{}/fund/{id}", d.web_url);
            let comment_id = reply(format!(
                "**Bounty requested: {} {}**, open for {days} days.\n\n@{login}, [fund this bounty]({link}) to activate it (link valid 24 h).",
                display(amount, 2),
                token.as_str()
            ))
            .await?;
            intents::set_comment(&d.db.pool, &id, comment_id).await?;
            Ok("intent-created".into())
        }
    }
}
