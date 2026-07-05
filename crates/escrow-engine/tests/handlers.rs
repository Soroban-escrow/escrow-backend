mod common;

use chrono::{Duration, Utc};
use common::*;
use escrow_core::BountyStatus;
use escrow_db::{MIGRATOR, audit, bounties, disputes, intents, payouts, prs, users};
use escrow_engine::{EngineError, dispatch::dispatch_webhook};
use escrow_github::GithubError;
use escrow_testkit::seed;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;

fn comment_event(body: &str, user_id: i64, login: &str, kind: &str, number: i32, state: &str, is_pr: bool) -> Value {
    let mut issue = json!({ "number": number, "state": state });
    if is_pr {
        issue["pull_request"] = json!({ "url": "https://api.github.com/..." });
    }
    json!({
        "action": "created", "installation": { "id": seed::INSTALLATION },
        "repository": { "id": seed::REPO_ID, "full_name": seed::REPO },
        "issue": issue,
        "comment": { "id": 5000 + i64::from(number), "body": body, "user": { "id": user_id, "login": login, "type": kind } }
    })
}

fn say(body: &str, login: &str) -> Value {
    comment_event(body, 500, login, "User", 3, "open", false)
}

async fn world(pool: PgPool) -> World {
    let w = World::new(pool);
    seed::repo(&w.db).await;
    w.gh.allow("alice", "write");
    w.gh.allow("carol", "read");
    w
}

async fn run(w: &World, event: &str, payload: &Value) -> Result<String, EngineError> {
    dispatch_webhook(&w.dispatch(None), event, payload).await
}

// ---------------- /bounty commands ----------------

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_writer_funding_command_creates_a_link_and_announces_it(pool: PgPool) {
    let w = world(pool.clone()).await;
    assert_eq!(run(&w, "issue_comment", &say("/bounty 50 XLM 14d", "alice")).await.unwrap(), "intent-created");
    let c = &w.gh.comments()[0];
    assert_eq!((c.repo.as_str(), c.issue), (seed::REPO, 3));
    assert!(c.body.contains("**Bounty requested: 50.00 XLM**, open for 14 days."), "{}", c.body);
    let link = c.body.split("](").nth(1).unwrap().split(')').next().unwrap();
    let id = link.strip_prefix("https://app.example/fund/").unwrap();
    let i = intents::get(&pool, id).await.unwrap().unwrap();
    assert_eq!((i.amount_i128(), i.token.as_str(), i.requested_by, i.duration_days, i.comment_id), (500_000_000, TOKEN_XLM, 500, 14, c.id));
    assert!(i.used_at.is_none());
    let ttl = i.expires_at - Utc::now();
    assert!(ttl > Duration::hours(23) && ttl <= Duration::hours(24));
    assert!(id.len() >= 30, "the id is an unguessable capability");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn the_default_token_and_duration_apply(pool: PgPool) {
    let w = world(pool.clone()).await;
    run(&w, "issue_comment", &say("/bounty 25", "alice")).await.unwrap();
    assert!(w.gh.comments()[0].body.contains("25.00 USDC") && w.gh.comments()[0].body.contains("14 days"));
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn only_people_with_write_access_can_fund(pool: PgPool) {
    let w = world(pool.clone()).await;
    for who in ["carol", "stranger"] {
        assert_eq!(run(&w, "issue_comment", &say("/bounty 50", who)).await.unwrap(), "denied");
    }
    assert!(w.gh.bodies().iter().all(|b| b.contains("only collaborators with write access can fund a bounty")));
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT count(*) FROM funding_intents").fetch_one(&pool).await.unwrap(), 0);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn bots_plain_comments_and_missing_installations_are_ignored(pool: PgPool) {
    let w = world(pool.clone()).await;
    let bot = comment_event("/bounty 50", 1, "dependabot[bot]", "Bot", 3, "open", false);
    assert_eq!(run(&w, "issue_comment", &bot).await.unwrap(), "ignored:bot");
    assert_eq!(run(&w, "issue_comment", &say("thanks, looks great", "alice")).await.unwrap(), "ignored:not-a-command");
    let mut no_inst = say("/bounty 50", "alice");
    no_inst.as_object_mut().unwrap().remove("installation");
    assert_eq!(run(&w, "issue_comment", &no_inst).await.unwrap(), "ignored:no-installation");
    let mut edited = say("/bounty 50", "alice");
    edited["action"] = json!("edited");
    assert_eq!(run(&w, "issue_comment", &edited).await.unwrap(), "ignored");
    assert!(w.gh.comments().is_empty());
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn funding_is_refused_with_a_clear_reason(pool: PgPool) {
    let w = world(pool.clone()).await;
    let on_pr = comment_event("/bounty 50", 500, "alice", "User", 8, "open", true);
    assert_eq!(run(&w, "issue_comment", &on_pr).await.unwrap(), "rejected:pr");
    let closed = comment_event("/bounty 50", 500, "alice", "User", 3, "closed", false);
    assert_eq!(run(&w, "issue_comment", &closed).await.unwrap(), "rejected:closed");
    assert_eq!(run(&w, "issue_comment", &say("/bounty 0.5", "alice")).await.unwrap(), "rejected:amount");
    assert_eq!(run(&w, "issue_comment", &say("/bounty 501", "alice")).await.unwrap(), "rejected:amount");
    assert_eq!(run(&w, "issue_comment", &say("/bounty 5 91d", "alice")).await.unwrap(), "rejected:duration");
    let bodies = w.gh.bodies();
    assert!(bodies[2].contains("Amount must be between 1 and 500 USDC."), "{}", bodies[2]);
    assert!(bodies[4].contains("Duration must be between 1 and 90 days."));

    seed::bounty(&w.db, &seed::B::new(4, 3)).await;
    assert_eq!(run(&w, "issue_comment", &say("/bounty 50", "alice")).await.unwrap(), "rejected:active");
    // a paid bounty no longer blocks a new one
    sqlx::query("UPDATE bounties SET status = 'RELEASED'").execute(&pool).await.unwrap();
    assert_eq!(run(&w, "issue_comment", &say("/bounty 50", "alice")).await.unwrap(), "intent-created");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_token_the_deployment_has_not_configured_is_refused(pool: PgPool) {
    let w = world(pool.clone()).await;
    let mut d = w.dispatch(None);
    d.commands.tokens.usdc = None;
    assert_eq!(dispatch_webhook(&d, "issue_comment", &say("/bounty 50 USDC", "alice")).await.unwrap(), "rejected:token");
    assert!(w.gh.bodies()[0].contains("USDC bounties are not enabled"));
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn help_and_usage_errors_reply_with_the_command_list(pool: PgPool) {
    let w = world(pool.clone()).await;
    assert_eq!(run(&w, "issue_comment", &say("/bounty help", "stranger")).await.unwrap(), "help");
    assert_eq!(run(&w, "issue_comment", &say("/bounty fifty", "stranger")).await.unwrap(), "error");
    let b = w.gh.bodies();
    assert!(b[0].contains("SkillEscrow commands") && b[1].contains("Invalid amount \"fifty\"") && b[1].contains("SkillEscrow commands"));
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_github_failure_while_replying_is_an_error_the_queue_can_retry(pool: PgPool) {
    let w = world(pool.clone()).await;
    w.gh.with(|s| s.fail_next = Some(GithubError::Status { status: 503, what: "x".into() }));
    let e = run(&w, "issue_comment", &say("/bounty help", "alice")).await.unwrap_err();
    assert!(e.is_transient());
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn disputes_come_only_from_the_maintainer_or_a_linked_pr_author(pool: PgPool) {
    let w = world(pool.clone()).await;
    assert_eq!(run(&w, "issue_comment", &say("/bounty dispute no way", "alice")).await.unwrap(), "rejected:no-bounty");
    seed::bounty(&w.db, &seed::B::new(4, 3)).await; // maintainer github id 500
    prs::upsert(
        &pool,
        &prs::PrUpsert { id: 100, number: 7, author_github_id: 900, author_login: "dev", head_sha: "a", state: "open", merged_at: None },
    )
    .await
    .unwrap();
    prs::link(&pool, 100, 4).await.unwrap();

    let stranger = comment_event("/bounty dispute unfair", 777, "eve", "User", 3, "open", false);
    assert_eq!(run(&w, "issue_comment", &stranger).await.unwrap(), "denied");
    let author = comment_event("/bounty dispute  it was reviewed unfairly  ", 900, "dev", "User", 3, "open", false);
    assert_eq!(run(&w, "issue_comment", &author).await.unwrap(), "dispute-requested");
    assert_eq!(run(&w, "issue_comment", &say("/bounty dispute duplicate PR won", "alice")).await.unwrap(), "dispute-requested");

    let open = disputes::list_open(&pool).await.unwrap();
    assert_eq!(open.len(), 2);
    assert_eq!(
        (open[0].opened_by_github_id, open[0].reason.as_str(), open[0].status.as_str()),
        (900, "it was reviewed unfairly", "requested")
    );
    assert!(audit::exists(&pool, "dispute.requested", "bounty:4").await.unwrap());
    assert!(w.gh.bodies().last().unwrap().contains("A human will review this within 48 hours"));
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_review_request_is_forced_and_limited_to_writers_on_linked_prs(pool: PgPool) {
    let w = world(pool.clone()).await;
    let sched = Arc::new(Scheduler::default());
    let d = w.dispatch(Some(sched.clone()));
    seed::bounty(&w.db, &seed::B::new(4, 3)).await;
    prs::upsert(
        &pool,
        &prs::PrUpsert { id: 100, number: 7, author_github_id: 900, author_login: "dev", head_sha: "a", state: "open", merged_at: None },
    )
    .await
    .unwrap();
    prs::link(&pool, 100, 4).await.unwrap();

    let on_issue = say("/bounty review", "alice");
    assert_eq!(dispatch_webhook(&d, "issue_comment", &on_issue).await.unwrap(), "ignored:review-not-on-issues");
    let by_reader = comment_event("/bounty review", 1, "carol", "User", 7, "open", true);
    assert_eq!(dispatch_webhook(&d, "issue_comment", &by_reader).await.unwrap(), "denied");
    let unlinked = comment_event("/bounty review", 500, "alice", "User", 99, "open", true);
    assert_eq!(dispatch_webhook(&d, "issue_comment", &unlinked).await.unwrap(), "rejected:not-linked");
    let ok = comment_event("/bounty review", 500, "alice", "User", 7, "open", true);
    assert_eq!(dispatch_webhook(&d, "issue_comment", &ok).await.unwrap(), "review-requested");
    assert_eq!(*sched.0.lock().unwrap(), [(100, std::time::Duration::ZERO, true)]);

    // with AI review switched off the command does nothing
    assert_eq!(run(&w, "issue_comment", &ok).await.unwrap(), "ignored:reviews-disabled");
}

// ---------------- pull requests ----------------

#[allow(clippy::too_many_arguments)] // a flat fixture: each test names the few facts it varies
fn pr_event(action: &str, number: i32, id: i64, author: i64, body: &str, merged: bool, draft: bool, state: &str) -> Value {
    json!({
        "action": action, "repository": { "id": seed::REPO_ID, "full_name": seed::REPO },
        "pull_request": { "id": id, "number": number, "body": body, "draft": draft, "merged": merged,
            "merged_at": if merged { json!("2026-11-03T10:00:00Z") } else { Value::Null }, "state": state,
            "head": { "sha": "abc1234" }, "user": { "id": author, "login": format!("user{author}"), "type": "User" } }
    })
}

async fn fund_bounty(w: &World, id: i64, issue: i32) {
    seed::bounty(&w.db, &seed::B::new(id, issue)).await;
    w.chain.put(id as u64, seed::MAINTAINER_WALLET, BountyStatus::Open, true, None);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn merging_a_pr_that_closes_a_bounty_starts_the_payout(pool: PgPool) {
    let w = world(pool.clone()).await;
    fund_bounty(&w, 4, 3).await;
    seed::user(&w.db, 900, "dev", Some("GCONTRIBUTOR")).await;
    assert_eq!(
        run(&w, "pull_request", &pr_event("closed", 7, 100, 900, "Closes #3", true, false, "closed")).await.unwrap(),
        "pull_request:1"
    );
    let p = payouts::for_bounty(&pool, 4).await.unwrap().unwrap();
    assert_eq!((p.status.as_str(), p.winner_address.as_deref(), p.pr_number), ("QUEUED", Some("GCONTRIBUTOR"), 7));
    let pr = prs::get(&pool, 100).await.unwrap().unwrap();
    assert_eq!((pr.state.as_str(), pr.author_login.as_str()), ("merged", "user900"));
    assert_eq!(prs::for_bounty(&pool, 4).await.unwrap().len(), 1);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_contributor_without_a_wallet_is_told_on_the_pr(pool: PgPool) {
    let w = world(pool.clone()).await;
    fund_bounty(&w, 4, 3).await;
    run(&w, "pull_request", &pr_event("closed", 7, 100, 900, "Fixes #3", true, false, "closed")).await.unwrap();
    assert_eq!(payouts::for_bounty(&pool, 4).await.unwrap().unwrap().status, "PENDING_WALLET");
    let c = &w.gh.comments()[0];
    assert_eq!(c.issue, 7, "the nudge goes on the PR");
    assert!(c.body.contains("Link your wallet at https://app.example/me"));
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn one_pr_can_close_two_bounties_and_both_are_paid(pool: PgPool) {
    let w = world(pool.clone()).await;
    fund_bounty(&w, 4, 3).await;
    fund_bounty(&w, 5, 4).await;
    seed::user(&w.db, 900, "dev", Some("GCONTRIBUTOR")).await;
    run(&w, "pull_request", &pr_event("closed", 7, 100, 900, "Closes #3 and fixes #4", true, false, "closed")).await.unwrap();
    for b in [4, 5] {
        assert_eq!(payouts::for_bounty(&pool, b).await.unwrap().unwrap().status, "QUEUED", "bounty {b}");
    }
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn the_maintainers_own_pr_never_earns_the_bounty(pool: PgPool) {
    let w = world(pool.clone()).await;
    fund_bounty(&w, 4, 3).await; // maintainer github id 500
    assert_eq!(
        run(&w, "pull_request", &pr_event("closed", 7, 100, 500, "Closes #3", true, false, "closed")).await.unwrap(),
        "pull_request:0"
    );
    assert!(payouts::for_bounty(&pool, 4).await.unwrap().is_none());
    assert!(prs::get(&pool, 100).await.unwrap().is_none());
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn prs_that_do_not_close_a_live_bounty_in_this_repo_are_ignored(pool: PgPool) {
    let w = world(pool.clone()).await;
    fund_bounty(&w, 4, 3).await;
    for body in ["Closes other/repo#3", "See #3", "Closes #99", ""] {
        assert_eq!(
            run(&w, "pull_request", &pr_event("closed", 7, 100, 900, body, true, false, "closed")).await.unwrap(),
            "pull_request:0",
            "{body:?}"
        );
    }
    sqlx::query("UPDATE bounties SET status = 'REFUNDED'").execute(&pool).await.unwrap();
    assert_eq!(
        run(&w, "pull_request", &pr_event("closed", 7, 100, 900, "Closes #3", true, false, "closed")).await.unwrap(),
        "pull_request:0"
    );
    assert!(payouts::for_bounty(&pool, 4).await.unwrap().is_none());
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn closing_without_merging_records_the_state_and_pays_nothing(pool: PgPool) {
    let w = world(pool.clone()).await;
    fund_bounty(&w, 4, 3).await;
    run(&w, "pull_request", &pr_event("opened", 7, 100, 900, "Closes #3", false, false, "open")).await.unwrap();
    run(&w, "pull_request", &pr_event("closed", 7, 100, 900, "Closes #3", false, false, "closed")).await.unwrap();
    assert_eq!(prs::get(&pool, 100).await.unwrap().unwrap().state, "closed");
    assert!(payouts::for_bounty(&pool, 4).await.unwrap().is_none());
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_merged_prs_record_is_frozen(pool: PgPool) {
    let w = world(pool.clone()).await;
    fund_bounty(&w, 4, 3).await;
    seed::user(&w.db, 900, "dev", Some("GCONTRIBUTOR")).await;
    run(&w, "pull_request", &pr_event("closed", 7, 100, 900, "Closes #3", true, false, "closed")).await.unwrap();
    // a late "synchronize"/reopen event, or a redelivery, must not rewrite history or pay twice
    assert_eq!(
        run(&w, "pull_request", &pr_event("reopened", 7, 100, 900, "Closes #3", false, false, "open")).await.unwrap(),
        "pull_request:0"
    );
    assert_eq!(prs::get(&pool, 100).await.unwrap().unwrap().state, "merged");
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT count(*) FROM payouts").fetch_one(&pool).await.unwrap(), 1);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn the_second_merged_pr_gets_no_second_payout(pool: PgPool) {
    let w = world(pool.clone()).await;
    fund_bounty(&w, 4, 3).await;
    seed::user(&w.db, 900, "dev", Some("GA")).await;
    seed::user(&w.db, 901, "dev2", Some("GB")).await;
    run(&w, "pull_request", &pr_event("closed", 7, 100, 900, "Closes #3", true, false, "closed")).await.unwrap();
    run(&w, "pull_request", &pr_event("closed", 8, 101, 901, "Closes #3", true, false, "closed")).await.unwrap();
    assert_eq!(payouts::for_bounty(&pool, 4).await.unwrap().unwrap().winner_github_id, 900);
    assert!(w.gh.bodies().iter().any(|b| b.contains("already claimed")));
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn reviews_are_scheduled_after_a_quiet_period_for_open_non_draft_prs_only(pool: PgPool) {
    let w = world(pool.clone()).await;
    fund_bounty(&w, 4, 3).await;
    let sched = Arc::new(Scheduler::default());
    let d = w.dispatch(Some(sched.clone()));
    let send = |ev: Value| {
        let d = d.clone();
        async move { dispatch_webhook(&d, "pull_request", &ev).await }
    };
    send(pr_event("opened", 7, 100, 900, "Closes #3", false, false, "open")).await.unwrap();
    send(pr_event("synchronize", 7, 100, 900, "Closes #3", false, false, "open")).await.unwrap();
    send(pr_event("opened", 8, 101, 901, "Closes #3", false, true, "open")).await.unwrap(); // draft
    send(pr_event("labeled", 7, 100, 900, "Closes #3", false, false, "open")).await.unwrap(); // not a content change
    let got = sched.0.lock().unwrap().clone();
    assert_eq!(got, [(100, std::time::Duration::from_secs(300), false), (100, std::time::Duration::from_secs(300), false)]);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn editing_a_pr_to_add_a_closing_keyword_links_it(pool: PgPool) {
    let w = world(pool.clone()).await;
    fund_bounty(&w, 4, 3).await;
    assert_eq!(
        run(&w, "pull_request", &pr_event("opened", 7, 100, 900, "Work in progress", false, false, "open")).await.unwrap(),
        "pull_request:0"
    );
    assert_eq!(
        run(&w, "pull_request", &pr_event("edited", 7, 100, 900, "Closes #3", false, false, "open")).await.unwrap(),
        "pull_request:1"
    );
    assert_eq!(prs::for_bounty(&pool, 4).await.unwrap().len(), 1);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn bot_prs_are_ignored(pool: PgPool) {
    let w = world(pool.clone()).await;
    fund_bounty(&w, 4, 3).await;
    let mut ev = pr_event("opened", 7, 100, 900, "Closes #3", false, false, "open");
    ev["pull_request"]["user"]["type"] = json!("Bot");
    assert_eq!(run(&w, "pull_request", &ev).await.unwrap(), "pull_request:0");
}

// ---------------- installations and issues ----------------

#[sqlx::test(migrator = "MIGRATOR")]
async fn installing_the_app_registers_the_account_and_its_repositories(pool: PgPool) {
    let w = World::new(pool.clone());
    let ev = json!({ "action": "created", "installation": { "id": 77, "account": { "login": "newco", "type": "Organization" } },
        "repositories": [{ "id": 1, "full_name": "newco/a", "private": false }, { "id": 2, "full_name": "newco/b", "private": true }] });
    assert_eq!(run(&w, "installation", &ev).await.unwrap(), "installation.created");
    assert_eq!(escrow_db::github::list_repos(&pool).await.unwrap().len(), 2);
    assert!(escrow_db::github::get_repo(&pool, 2).await.unwrap().unwrap().is_private);

    let added = json!({ "action": "added", "installation": { "id": 77, "account": { "login": "newco", "type": "Organization" } },
        "repositories_added": [{ "id": 3, "full_name": "newco/c", "private": false }] });
    run(&w, "installation_repositories", &added).await.unwrap();
    assert_eq!(escrow_db::github::list_repos(&pool).await.unwrap().len(), 3);

    // suspend and uninstall keep the rows: bounties still point at them
    for action in ["suspend", "deleted"] {
        let ev = json!({ "action": action, "installation": { "id": 77, "account": { "login": "newco", "type": "Organization" } } });
        run(&w, "installation", &ev).await.unwrap();
        assert!(escrow_db::github::get_installation(&pool, 77).await.unwrap().unwrap().suspended_at.is_some());
    }
    assert_eq!(escrow_db::github::list_repos(&pool).await.unwrap().len(), 3);
    let back = json!({ "action": "unsuspend", "installation": { "id": 77, "account": { "login": "newco", "type": "Organization" } } });
    run(&w, "installation", &back).await.unwrap();
    assert!(escrow_db::github::get_installation(&pool, 77).await.unwrap().unwrap().suspended_at.is_none());
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn editing_an_issue_refreshes_the_live_bounties_title_and_criteria(pool: PgPool) {
    let w = world(pool.clone()).await;
    fund_bounty(&w, 4, 3).await;
    fund_bounty(&w, 5, 4).await;
    sqlx::query("UPDATE bounties SET status = 'RELEASED' WHERE id = 5").execute(&pool).await.unwrap();
    let ev = json!({ "action": "edited", "repository": { "id": seed::REPO_ID }, "issue": { "number": 3, "title": "New title", "body": "## Acceptance criteria\n- a\n- b" } });
    assert_eq!(run(&w, "issues", &ev).await.unwrap(), "issues.edited:1");
    let b = bounties::get(&pool, 4).await.unwrap().unwrap();
    assert_eq!((b.issue_title.as_str(), b.criteria.clone()), ("New title", vec!["a".to_string(), "b".to_string()]));
    let ev5 = json!({ "action": "edited", "repository": { "id": seed::REPO_ID }, "issue": { "number": 4, "title": "x", "body": "" } });
    assert_eq!(run(&w, "issues", &ev5).await.unwrap(), "issues.edited:0", "a paid bounty's record is not rewritten");
    let opened = json!({ "action": "opened", "repository": { "id": 1 }, "issue": { "number": 3, "title": "t" } });
    assert_eq!(run(&w, "issues", &opened).await.unwrap(), "ignored");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn unknown_events_are_ignored_and_malformed_payloads_are_permanent_errors(pool: PgPool) {
    let w = world(pool.clone()).await;
    assert_eq!(run(&w, "star", &json!({})).await.unwrap(), "ignored");
    assert_eq!(run(&w, "ping", &json!({ "zen": "x" })).await.unwrap(), "ignored");
    let e = run(&w, "pull_request", &json!({ "action": "closed" })).await.unwrap_err();
    assert!(matches!(e, EngineError::Payload(_)) && !e.is_transient(), "retrying a bad payload cannot help");
    let _ = users::get(&pool, "x").await;
}
