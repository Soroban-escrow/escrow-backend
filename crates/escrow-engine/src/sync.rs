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
use escrow_db::{
    Db, audit, bounties, github as gh_repo,
    models::{Bounty, Repo},
    payout_reminders, payouts, prs, users,
};
use escrow_github::GithubApi;
use escrow_payouts::Notifier;
use sqlx::PgPool;
use std::sync::Arc;

async fn active_repo(pool: &PgPool, bounty_id: i64) -> Result<Option<(Bounty, Repo)>, EngineError> {
    let Some((bounty, repo)) = bounties::with_repo(pool, bounty_id).await? else {
        return Ok(None);
    };
    if repo.suspended_at.is_some() {
        tracing::warn!(bounty = bounty_id, repo = %repo.full_name, "skipping GitHub call for inactive repository");
        return Ok(None);
    }
    Ok(Some((bounty, repo)))
}

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
///
/// The database transaction is NOT held across GitHub API calls. Instead we use a short transaction
/// only to check/set the card comment ID under an advisory lock, then perform the GitHub call,
/// then a second short transaction to persist the comment ID if it was a new post.
pub async fn refresh_bounty_card(d: &SyncDeps, bounty_id: i64) -> Result<CardOutcome, EngineError> {
    let Some((b, repo)) = active_repo(&d.db.pool, bounty_id).await? else {
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

    // Short transaction 1: acquire advisory lock, check if card comment exists.
    // If it exists, we'll update it. If not, we'll post a new one.
    let (outcome, existing_cid) = {
        let mut tx = d.db.pool.begin().await?;
        bounties::lock_card(&mut tx, b.id).await?;
        match bounties::card_comment(&mut *tx, b.id).await? {
            Some(cid) => {
                tx.commit().await?;
                (CardOutcome::Updated, Some(cid))
            }
            None => {
                tx.commit().await?;
                (CardOutcome::Posted, None)
            }
        }
    };

    // GitHub API call OUTSIDE any database transaction.
    let comment_id = match existing_cid {
        Some(cid) => {
            d.gh.update_comment(inst, name, cid, &body).await?;
            cid
        }
        None => {
            let cid = d.gh.post_comment(inst, name, b.issue_number, &body).await?;

            // Short transaction 2: persist the comment ID under the lock.
            // Double-check in case another concurrent refresh already posted.
            let mut tx = d.db.pool.begin().await?;
            bounties::lock_card(&mut tx, b.id).await?;
            if let Some(existing) = bounties::card_comment(&mut *tx, b.id).await? {
                tx.commit().await?;
                // Another process already posted; update that comment instead of leaving two.
                d.gh.update_comment(inst, name, existing, &body).await?;
                existing
            } else {
                bounties::set_card_comment(&mut *tx, b.id, cid).await?;
                tx.commit().await?;
                cid
            }
        }
    };
    let _ = comment_id; // silences unused warning when tracing is off

    if b.status().is_active() {
        d.gh.set_bounty_label(inst, name, b.issue_number, &label_for(b.amount_i128(), &symbol)).await?;
    } else {
        d.gh.remove_bounty_label(inst, name, b.issue_number).await?;
    }
    Ok(outcome)
}

/// "Paid 4.85 XLM to @login (tx)" on the PR, once per bounty. Covers every release path (relayer or the
/// maintainer signing). Marked in the audit log so a replayed event never comments twice.
pub async fn announce_payout(d: &SyncDeps, bounty_id: i64) -> Result<bool, EngineError> {
    let Some((b, repo)) = active_repo(&d.db.pool, bounty_id).await? else {
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
    // a stored fee or amount outside the contract's bounds is a data error: say so instead of posting a wrong amount
    let net = fees::net(b.amount_i128(), b.fee_bps as u32).map_err(|e| EngineError::Other(format!("fee for bounty {}: {e}", b.id)))?;
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
        let Some((b, repo)) = active_repo(&self.db.pool, bounty_id).await.map_err(|e| e.to_string())? else {
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
        if let Some((b, repo)) = active_repo(&self.db.pool, bounty_id).await? {
            self.gh.post_comment(repo.installation_id, &repo.full_name, b.issue_number, body).await?;
        }
        Ok(())
    }
}

/// Text of the dispute outcome comment. `note` is only meaningful for an admin rejection.
pub fn dispute_comment_body(kind: &str, note: Option<&str>) -> String {
    match kind {
        "disputed" => "A dispute was opened on chain for this bounty. It is awaiting review by an admin.".into(),
        "resolved" => "The dispute on this bounty was resolved on chain.".into(),
        "rejected" => format!("The dispute on this bounty was rejected by the arbiter: {}", note.unwrap_or_default()),
        other => note.unwrap_or(other).to_owned(),
    }
}

/// Posts a dispute outcome on the bounty's issue at most once per event/decision. The audit marker is
/// written only after GitHub accepts the comment, so a failed call is retried by the job queue.
pub async fn post_dispute_comment(d: &SyncDeps, bounty_id: i64, body: &str, key: &str) -> Result<bool, EngineError> {
    let target = format!("dispute.comment:{key}");
    if audit::exists(&d.db.pool, "dispute.commented", &target).await? {
        return Ok(false);
    }
    if let Some((b, repo)) = active_repo(&d.db.pool, bounty_id).await? {
        d.gh.post_comment(repo.installation_id, &repo.full_name, b.issue_number, body).await?;
    }
    audit::log(&d.db.pool, "system", "dispute.commented", &target, &serde_json::json!({ "body": body })).await?;
    Ok(true)
}

/// What one reminder pass did: the reminders that went out and the ones whose comment failed, each as `kind:bounty_id`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReminderReport {
    pub sent: Vec<String>,
    /// Failed comments are not recorded as sent, so the next pass retries them.
    pub failed: Vec<String>,
}

impl ReminderReport {
    /// True when the pass neither sent nor failed anything.
    pub fn is_empty(&self) -> bool {
        self.sent.is_empty() && self.failed.is_empty()
    }
}

/// Refund reminders: one 48 h before the refund window opens (deadline + grace), one when it opens.
/// Each is sent at most once, recorded in the audit log AFTER the comment succeeds (a failed comment is retried next tick).
/// A comment that fails (a revoked installation, say) is logged and reported in `failed`; it does not stop the other bounties.
pub async fn send_refund_reminders(
    db: &Db,
    commenter: &dyn IssueCommenter,
    grace: Duration,
    web_url: &str,
    now: DateTime<Utc>,
) -> Result<ReminderReport, EngineError> {
    let mut report = ReminderReport::default();
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
        // #103: prefix the reminder with an @mention when we know the maintainer's login, so
        // the person who needs to act is pinged on GitHub rather than relying on them watching
        // the issue. A missing login (unlinked maintainer wallet, closed account) leaves the
        // text unchanged - the reminder still goes out, just without the mention.
        let mention = match b.maintainer_github_id {
            Some(id) => users::by_github_id(&db.pool, id).await?.map(|u| format!("@{} ", u.github_login)).unwrap_or_default(),
            None => String::new(),
        };
        let body = if kind == "soon" {
            format!(
                "{mention}The refund window for this bounty opens in under 48 hours. If a PR was merged but not yet paid, resolve it at {web_url}/b/{}; otherwise the maintainer may reclaim the funds.",
                b.id
            )
        } else {
            format!(
                "{mention}The refund window for this bounty is now open. The maintainer can reclaim the funds at {web_url}/me. A merged PR can still be released until then."
            )
        };
        if let Err(e) = commenter.comment(b.id, &body).await {
            tracing::warn!(bounty = b.id, kind, error = %e, "refund reminder failed; the other bounties still get theirs");
            report.failed.push(format!("{kind}:{}", b.id));
            continue;
        }
        audit::log(&db.pool, "system", &action, &target, &serde_json::json!({})).await?;
        report.sent.push(format!("{kind}:{}", b.id));
    }
    Ok(report)
}

/// Wallet reminder stages after a PR merge while its payout waits for a wallet: name and hours after the merge.
/// One comment per stage, never more than three.
const WALLET_STAGES: [(&str, i64); 3] = [("h1", 1), ("h24", 24), ("h72", 72)];

/// The next stage due for a payout `age` old that already has `sent` stages. Stages go out in order.
pub fn due_wallet_stage(age: Duration, sent: &[String]) -> Option<&'static str> {
    WALLET_STAGES.iter().find(|(s, h)| age >= Duration::hours(*h) && !sent.iter().any(|x| x == s)).map(|(s, _)| *s)
}

/// The comment asking a contributor to link a wallet. Only the login and the numbers come from outside.
pub fn wallet_reminder_body(login: &str, amount: &str, symbol: &str, bounty_id: i64, web_url: &str, window_opens: DateTime<Utc>) -> String {
    format!(
        "@{login} your merged PR is waiting for a wallet. **{amount} {symbol}** is held for you. Link a wallet at {web_url}/me to receive it. If no wallet is linked before {}, the maintainer may reclaim the funds (bounty #{bounty_id}).",
        window_opens.format("%Y-%m-%d %H:%M UTC")
    )
}

/// The one-time confirmation after a contributor who was reminded has linked a wallet.
pub fn wallet_linked_body(login: &str) -> String {
    format!("@{login} your wallet is linked. Your payout is queued and will be paid shortly.")
}

/// Wallet reminders for merged PRs whose payout is still waiting for a wallet. Runs on the hourly tick.
/// Each stage is recorded after its comment succeeds, so a restart never repeats one. Stops when the bounty is no
/// longer open or its refund window has opened. A bot or a deleted account is skipped and logged to the audit trail.
pub async fn send_wallet_reminders(d: &SyncDeps, grace: Duration, now: DateTime<Utc>) -> Result<Vec<String>, EngineError> {
    let mut sent = vec![];
    for p in payout_reminders::waiting(&d.db.pool).await? {
        let Some((b, repo)) = active_repo(&d.db.pool, p.bounty_id).await? else {
            continue;
        };
        if !b.status().is_active() || now >= b.deadline + grace {
            continue;
        }
        if !payout_reminders::wallet_reminders_enabled(&d.db.pool, repo.id).await? {
            continue;
        }
        let done = payout_reminders::stages_sent(&d.db.pool, &p.payout_id).await?;
        let Some(stage) = due_wallet_stage(now - p.created_at, &done) else {
            continue;
        };
        let Some(pr) = prs::for_bounty(&d.db.pool, b.id).await?.into_iter().find(|l| l.number == p.pr_number) else {
            continue;
        };
        let target = format!("payout:{}", p.payout_id);
        let outcome = if pr.author_login.ends_with("[bot]") {
            "skipped"
        } else {
            let net = match fees::net(b.amount_i128(), b.fee_bps as u32) {
                Ok(net) => net,
                Err(e) => {
                    // one bad row must not stop the reminders for every other payout
                    tracing::warn!(bounty = b.id, error = %e, "wallet reminder skipped: payout amount or fee is out of bounds");
                    continue;
                }
            };
            let body = wallet_reminder_body(
                &pr.author_login,
                &display(net, 2),
                &d.tokens.symbol_for(&b.token),
                b.id,
                &d.web_url,
                b.deadline + grace,
            );
            match d.gh.post_comment(repo.installation_id, &repo.full_name, p.pr_number, &body).await {
                Ok(_) => "sent",
                // the account is gone or the PR is unreachable: nothing to retry, so record it and tell the maintainers
                Err(e) if e.status() == Some(404) => "skipped",
                Err(e) => return Err(e.into()),
            }
        };
        let recorded = if outcome == "sent" { stage } else { "skipped" };
        payout_reminders::record(&d.db.pool, &p.payout_id, recorded).await?;
        audit::log(&d.db.pool, "system", &format!("wallet_reminder.{outcome}"), &target, &serde_json::json!({ "stage": stage })).await?;
        if outcome == "sent" {
            sent.push(format!("{stage}:{}", p.payout_id));
        } else {
            tracing::warn!(payout = %p.payout_id, "wallet reminder skipped; maintainers should follow up");
        }
    }
    for p in payout_reminders::linked_after_reminder(&d.db.pool).await? {
        let Some((b, repo)) = active_repo(&d.db.pool, p.bounty_id).await? else {
            continue;
        };
        let Some(pr) = prs::for_bounty(&d.db.pool, b.id).await?.into_iter().find(|l| l.number == p.pr_number) else {
            continue;
        };
        d.gh.post_comment(repo.installation_id, &repo.full_name, p.pr_number, &wallet_linked_body(&pr.author_login)).await?;
        payout_reminders::record(&d.db.pool, &p.payout_id, "linked").await?;
        sent.push(format!("linked:{}", p.payout_id));
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
    fn wallet_stages_go_out_in_order_and_stop_at_three() {
        let h = Duration::hours;
        assert_eq!(due_wallet_stage(h(0), &[]), None);
        assert_eq!(due_wallet_stage(h(1), &[]), Some("h1"));
        assert_eq!(due_wallet_stage(h(30), &["h1".into()]), Some("h24"));
        assert_eq!(due_wallet_stage(h(200), &["h1".into(), "h24".into()]), Some("h72"));
        assert_eq!(due_wallet_stage(h(200), &["h1".into(), "h24".into(), "h72".into()]), None);
    }

    #[test]
    fn wallet_reminder_text_is_stable() {
        let when = DateTime::parse_from_rfc3339("2026-11-20T12:00:00Z").unwrap().with_timezone(&Utc);
        assert_eq!(
            wallet_reminder_body("octocat", "4.85", "USDC", 7, "https://app.example", when),
            "@octocat your merged PR is waiting for a wallet. **4.85 USDC** is held for you. Link a wallet at https://app.example/me to receive it. If no wallet is linked before 2026-11-20 12:00 UTC, the maintainer may reclaim the funds (bounty #7)."
        );
    }

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
