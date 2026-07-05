//! Keeps the bounty's GitHub presence in step with the chain: the card, the label, the "Paid" comment,
//! the payout notifications and the refund reminders. All of it is best effort and re-runnable; the chain is the truth.

use crate::error::EngineError;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use escrow_core::{
    amounts::display,
    card::{CardInput, label_for, render_card},
    criteria::parse_criteria,
    fees,
};
use escrow_db::{Db, audit, bounties, github as gh_repo, payouts, prs};
use escrow_github::GithubApi;
use escrow_payouts::Notifier;
use std::sync::Arc;

/// Token contract ids, to show symbols instead of addresses.
#[derive(Clone, Default)]
pub struct Tokens {
    pub usdc: String,
    pub xlm: String,
}

impl Tokens {
    pub fn symbol_for(&self, token: &str) -> String {
        if !self.usdc.is_empty() && token == self.usdc {
            "USDC".into()
        } else if !self.xlm.is_empty() && token == self.xlm {
            "XLM".into()
        } else {
            format!("{}…", &token[..token.len().min(4)])
        }
    }
}

#[derive(Clone)]
pub struct SyncDeps {
    pub db: Db,
    pub gh: Arc<dyn GithubApi>,
    pub web_url: String,
    pub tokens: Tokens,
    /// `https://explorer/.../{hash}`
    pub explorer_template: String,
}

impl SyncDeps {
    fn explorer(&self, hash: &str) -> String {
        self.explorer_template.replace("{hash}", hash)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum CardOutcome {
    Posted,
    Updated,
    Skipped,
}

/// Fills the issue title and criteria, posts the card once and edits it in place afterwards, and keeps the
/// bounty label current. Safe to call repeatedly: every chain event for the bounty calls it.
pub async fn refresh_bounty_card(d: &SyncDeps, bounty_id: i64) -> Result<CardOutcome, EngineError> {
    let Some((b, repo)) = bounties::with_repo(&d.db.pool, bounty_id).await? else {
        return Ok(CardOutcome::Skipped);
    };
    let (inst, name) = (repo.installation_id, repo.full_name.as_str());

    // the indexer only knows what the chain says; the title and criteria come from the issue
    let (mut title, mut criteria) = (b.issue_title.clone(), b.criteria.clone());
    match d.gh.get_issue(inst, name, b.issue_number).await {
        Ok(issue) => {
            title = issue.title;
            criteria = parse_criteria(issue.body.as_deref());
            if title != b.issue_title || criteria != b.criteria {
                bounties::set_issue_data(&d.db.pool, b.id, &title, &criteria).await?;
            }
        }
        Err(e) => tracing::warn!(bounty = b.id, error = %e, "issue fetch failed; rendering with stored data"),
    }
    let _ = title;

    let symbol = d.tokens.symbol_for(&b.token);
    let body = render_card(&CardInput {
        id: b.id,
        status: b.status(),
        amount: b.amount_i128(),
        symbol: &symbol,
        deadline: b.deadline,
        relayer_allowed: b.relayer_allowed,
        criteria: &criteria,
        issue_number: b.issue_number,
        web_url: &d.web_url,
    });

    // One refresh at a time per bounty: without the lock two concurrent calls would both post a card.
    let mut tx = d.db.pool.begin().await?;
    bounties::lock_card(&mut tx, b.id).await?;
    let outcome = match bounties::card_comment(&mut *tx, b.id).await? {
        Some(cid) => {
            d.gh.update_comment(inst, name, cid, &body).await?;
            CardOutcome::Updated
        }
        None => {
            let cid = d.gh.post_comment(inst, name, b.issue_number, &body).await?;
            bounties::set_card_comment(&mut *tx, b.id, cid).await?;
            CardOutcome::Posted
        }
    };
    tx.commit().await?;

    if b.status().is_active() {
        d.gh.set_bounty_label(inst, name, b.issue_number, &label_for(b.amount_i128(), &symbol)).await?;
    }
    Ok(outcome)
}

/// "Paid 4.85 XLM to @login (tx)" on the PR, once per bounty. Covers every release path (relayer or the
/// maintainer signing). Marked in the audit log so a replayed event never comments twice.
pub async fn announce_payout(d: &SyncDeps, bounty_id: i64) -> Result<bool, EngineError> {
    let Some((b, repo)) = bounties::with_repo(&d.db.pool, bounty_id).await? else {
        return Ok(false);
    };
    let (Some(tx), Some(winner)) = (b.close_tx.as_deref(), b.winner_address.as_deref()) else {
        return Ok(false);
    };
    if b.status() != escrow_core::BountyStatus::Released {
        return Ok(false);
    }
    let target = format!("bounty:{}", b.id);
    if audit::exists(&d.db.pool, "payout.announced", &target).await? {
        return Ok(false);
    }
    let payout = payouts::for_bounty(&d.db.pool, b.id).await?;
    let linked = prs::for_bounty(&d.db.pool, b.id).await?;
    let pr = payout.as_ref().and_then(|p| linked.iter().find(|l| l.number == p.pr_number));
    let who = pr.map_or_else(
        || format!("`{}…{}`", &winner[..winner.len().min(6)], &winner[winner.len().saturating_sub(4)..]),
        |p| format!("@{}", p.author_login),
    );
    let net = fees::net(b.amount_i128(), b.fee_bps as u32).map_err(|_| EngineError::Other("fee overflow".into()))?;
    let body = format!(
        "**Paid {} {} to {who}.** [Transaction {}…]({})",
        display(net, 2),
        d.tokens.symbol_for(&b.token),
        &tx[..tx.len().min(8)],
        d.explorer(tx)
    );
    d.gh.post_comment(repo.installation_id, &repo.full_name, payout.as_ref().map_or(b.issue_number, |p| p.pr_number), &body).await?;
    audit::log(&d.db.pool, "system", "payout.announced", &target, &serde_json::json!({ "tx": tx })).await?;
    Ok(true)
}

/// Comments for the payout flow: on the PR when there is one, otherwise on the bounty's issue.
pub struct GithubNotifier {
    pub db: Db,
    pub gh: Arc<dyn GithubApi>,
}

#[async_trait]
impl Notifier for GithubNotifier {
    async fn notify(&self, bounty_id: i64, pr_number: i32, message: &str) -> Result<(), String> {
        let Some((b, repo)) = bounties::with_repo(&self.db.pool, bounty_id).await.map_err(|e| e.to_string())? else {
            return Ok(());
        };
        let target = if pr_number > 0 { pr_number } else { b.issue_number };
        self.gh.post_comment(repo.installation_id, &repo.full_name, target, message).await.map(|_| ()).map_err(|e| e.to_string())
    }
}

/// Posts a comment on a bounty's issue (reminders). The real one uses the GitHub App; tests use a fake.
#[async_trait]
pub trait IssueCommenter: Send + Sync {
    async fn comment(&self, bounty_id: i64, body: &str) -> Result<(), EngineError>;
}

pub struct GithubCommenter {
    pub db: Db,
    pub gh: Arc<dyn GithubApi>,
}

#[async_trait]
impl IssueCommenter for GithubCommenter {
    async fn comment(&self, bounty_id: i64, body: &str) -> Result<(), EngineError> {
        if let Some((b, repo)) = bounties::with_repo(&self.db.pool, bounty_id).await? {
            self.gh.post_comment(repo.installation_id, &repo.full_name, b.issue_number, body).await?;
        }
        Ok(())
    }
}

/// Refund reminders: one 48 h before the refund window opens (deadline + grace), one when it opens.
/// Each is sent at most once, recorded in the audit log AFTER the comment succeeds (a failed comment is retried next tick).
pub async fn send_refund_reminders(
    db: &Db,
    commenter: &dyn IssueCommenter,
    grace: Duration,
    web_url: &str,
    now: DateTime<Utc>,
) -> Result<Vec<String>, EngineError> {
    let mut sent = vec![];
    for b in bounties::open_ones(&db.pool).await? {
        let opens_at = b.deadline + grace;
        let kind = if now >= opens_at {
            "open"
        } else if now >= opens_at - Duration::hours(48) {
            "soon"
        } else {
            continue;
        };
        let (action, target) = (format!("reminder.refund_{kind}"), format!("bounty:{}", b.id));
        if audit::exists(&db.pool, &action, &target).await? {
            continue;
        }
        let body = if kind == "soon" {
            format!(
                "The refund window for this bounty opens in under 48 hours. If a PR was merged but not yet paid, resolve it at {web_url}/b/{}; otherwise the maintainer may reclaim the funds.",
                b.id
            )
        } else {
            format!(
                "The refund window for this bounty is now open. The maintainer can reclaim the funds at {web_url}/me. A merged PR can still be released until then."
            )
        };
        commenter.comment(b.id, &body).await?;
        audit::log(&db.pool, "system", &action, &target, &serde_json::json!({})).await?;
        sent.push(format!("{kind}:{}", b.id));
    }
    Ok(sent)
}

/// Brings installations and their repositories in line with GitHub (covers webhooks missed while we were down).
pub async fn sync_installations(db: &Db, gh: &dyn GithubApi) -> Result<(usize, usize), EngineError> {
    let (mut installs, mut repos) = (0, 0);
    for i in gh.list_installations().await? {
        gh_repo::upsert_installation(&db.pool, i.id, &i.account.login, &i.account.kind, i.suspended_at).await?;
        installs += 1;
        if i.suspended_at.is_some() {
            continue;
        }
        for r in gh.list_repositories(i.id).await? {
            gh_repo::upsert_repo(&db.pool, r.id, i.id, &r.full_name, r.private).await?;
            repos += 1;
        }
    }
    Ok((installs, repos))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbols_fall_back_to_a_short_id() {
        let t = Tokens { usdc: "CUSDC".into(), xlm: "CXLM".into() };
        assert_eq!(
            (t.symbol_for("CUSDC").as_str(), t.symbol_for("CXLM").as_str(), t.symbol_for("CABCDEF").as_str()),
            ("USDC", "XLM", "CABC…")
        );
        assert_eq!(Tokens::default().symbol_for(""), "…", "an unconfigured token never matches an empty id");
    }
}
