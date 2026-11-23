//! Routes one verified webhook delivery to its handler. Unknown events are ignored, not errors.

use crate::{
    commands::{CommandDeps, handle_issue_comment},
    error::EngineError,
    jobs,
    pulls::{PullDeps, handle_pull_request},
};
use escrow_core::criteria::parse_criteria;
use escrow_db::{Db, audit, bounties, github};
use escrow_github::payloads::{InstallationEvent, IssueCommentEvent, IssuesEvent, PullRequestEvent};
use serde_json::Value;

#[derive(Clone)]
pub struct DispatchDeps {
    pub db: Db,
    pub commands: CommandDeps,
    pub pulls: PullDeps,
}

fn parse<T: serde::de::DeserializeOwned>(payload: &Value, what: &str) -> Result<T, EngineError> {
    serde_json::from_value(payload.clone()).map_err(|e| EngineError::Payload(format!("{what}: {e}")))
}

/// Webhook events the backend actually handles; anything else is acknowledged but never queued.
/// Shared with the webhook route so unhandled deliveries do not cost a row and a job.
pub fn handles_event(event: &str) -> bool {
    matches!(event, "installation" | "installation_repositories" | "issue_comment" | "pull_request" | "issues")
}

pub async fn dispatch_webhook(d: &DispatchDeps, event: &str, payload: &Value) -> Result<String, EngineError> {
    let pool = &d.db.pool;
    match event {
        "installation" => {
            let p: InstallationEvent = parse(payload, "installation")?;
            let id = p.installation.id;
            let (login, kind) = p.installation.account.as_ref().map_or(("", ""), |a| (a.login.as_str(), a.kind.as_str()));
            match p.action.as_str() {
                "created" | "unsuspend" | "new_permissions_accepted" => {
                    github::upsert_installation(pool, id, login, kind, None).await?;
                    for r in &p.repositories {
                        github::upsert_repo(pool, r.id, id, &r.full_name, r.private).await?;
                    }
                }
                // keep the rows that bounties point at; only mark the installation suspended
                "suspend" | "deleted" => github::suspend_installation(pool, id, chrono::Utc::now()).await?,
                _ => {}
            }
            Ok(format!("installation.{}", p.action))
        }
        "installation_repositories" => {
            let p: InstallationEvent = parse(payload, "installation_repositories")?;
            let (login, kind) = p.installation.account.as_ref().map_or(("", ""), |a| (a.login.as_str(), a.kind.as_str()));
            github::ensure_installation(pool, p.installation.id, login, kind).await?;
            for r in &p.repositories_added {
                github::upsert_repo(pool, r.id, p.installation.id, &r.full_name, r.private).await?;
            }
            // Handle repositories_removed: mark them as deleted to prevent bounty creation
            for r in &p.repositories_removed {
                github::suspend_repo(pool, r.id).await?;
            }
            Ok(format!("installation_repositories.{}", p.action))
        }
        "issue_comment" => {
            let p: IssueCommentEvent = parse(payload, "issue_comment")?;
            if p.action != "created" {
                return Ok("ignored".into());
            }
            handle_issue_comment(&d.commands, &p).await
        }
        "pull_request" => {
            let p: PullRequestEvent = parse(payload, "pull_request")?;
            Ok(format!("pull_request:{}", handle_pull_request(&d.pulls, &p).await?.len()))
        }
        "issues" => handle_issues(pool, &parse(payload, "issues")?).await,
        _ => Ok("ignored".into()),
    }
}

/// Issue lifecycle events for bountied issues:
/// * `edited` refreshes the stored title and criteria on the live bounty.
/// * `closed` / `reopened` refresh the bounty card so it reflects the issue's new state.
/// * `transferred` / `deleted` orphan an active bounty (its issue moved or vanished); record an operator
///   alert so `collect_alerts` keeps surfacing it until someone acts.
async fn handle_issues(pool: &sqlx::PgPool, p: &IssuesEvent) -> Result<String, EngineError> {
    let repo_id = p.repository.id;
    let number = p.issue.number;
    match p.action.as_str() {
        "edited" => {
            let n =
                bounties::set_issue_data_for_issue(pool, repo_id, number, &p.issue.title, &parse_criteria(p.issue.body.as_deref())).await?;
            // Refresh bounty cards when issue details are edited so they display latest title/criteria
            let live = bounties::active_for_issues(pool, repo_id, &[number]).await?;
            for b in &live {
                jobs::enqueue_card_refresh(pool, b.id).await?;
            }
            Ok(format!("issues.edited:{n}"))
        }
        "closed" | "reopened" => {
            let live = bounties::active_for_issues(pool, repo_id, &[number]).await?;
            for b in &live {
                jobs::enqueue_card_refresh(pool, b.id).await?;
            }
            Ok(format!("issues.{}:{}", p.action, live.len()))
        }
        "transferred" | "deleted" => {
            let live = bounties::active_for_issues(pool, repo_id, &[number]).await?;
            for b in &live {
                // One alert per bounty per transition; `collect_alerts` reads these while the bounty is live.
                audit::log(
                    pool,
                    "github",
                    &format!("issue.{}.alert", p.action),
                    &format!("bounty:{}", b.id),
                    &serde_json::json!({ "repo_id": repo_id, "issue": number, "issue_url": b.issue_url }),
                )
                .await?;
            }
            Ok(format!("issues.{}:{}", p.action, live.len()))
        }
        _ => Ok("ignored".into()),
    }
}
