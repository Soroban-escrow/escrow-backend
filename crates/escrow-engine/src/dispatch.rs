//! Routes one verified webhook delivery to its handler. Unknown events are ignored, not errors.

use crate::{
    commands::{CommandDeps, handle_issue_comment},
    error::EngineError,
    pulls::{PullDeps, handle_pull_request},
};
use escrow_core::criteria::parse_criteria;
use escrow_db::{Db, bounties, github};
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
        "issues" => {
            let p: IssuesEvent = parse(payload, "issues")?;
            if p.action != "edited" {
                return Ok("ignored".into());
            }
            let n = bounties::set_issue_data_for_issue(
                pool,
                p.repository.id,
                p.issue.number,
                &p.issue.title,
                &parse_criteria(p.issue.body.as_deref()),
            )
            .await?;
            Ok(format!("issues.edited:{n}"))
        }
        _ => Ok("ignored".into()),
    }
}
