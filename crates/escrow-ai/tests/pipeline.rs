//! The review pipeline against real Postgres, a fake GitHub and a scripted model.

use async_trait::async_trait;
use chrono::{Duration, Utc};
use escrow_ai::{ModelOutput, ModelPort, ReviewDeps, ReviewOutcome, review::ModelError, run_pr_review};
use escrow_db::{Db, MIGRATOR, prs, reviews};
use escrow_github::CiConclusion;
use escrow_testkit::{FakeGithub, seed};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Model {
    answers: Mutex<Vec<Result<Value, ModelError>>>,
    calls: Mutex<usize>,
    default: Mutex<Option<Value>>,
}

impl Model {
    fn say(&self, v: Value) {
        self.answers.lock().unwrap().push(Ok(v));
    }
    fn always(&self, v: Value) {
        *self.default.lock().unwrap() = Some(v);
    }
    fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

#[async_trait]
impl ModelPort for Model {
    async fn complete(&self, _s: &str, _u: &str) -> Result<ModelOutput, ModelError> {
        *self.calls.lock().unwrap() += 1;
        let next = {
            let mut a = self.answers.lock().unwrap();
            if a.is_empty() { None } else { Some(a.remove(0)) }
        };
        let v = match next {
            Some(r) => r?,
            None => self.default.lock().unwrap().clone().expect("model script exhausted"),
        };
        Ok(ModelOutput { output: v, input_tokens: 500, output_tokens: 50 })
    }
}

fn answer(verdict: &str, score: i32, flags: &[&str]) -> Value {
    json!({ "verdict": verdict, "score": score, "criteria": [{ "criterion": "Retries", "derived": false, "status": "met", "evidence": "src/http.rs:4" }],
            "summary": "Looks reasonable.", "risks": [], "flags": flags, "questions_for_maintainer": [] })
}

struct Env {
    d: ReviewDeps,
    gh: Arc<FakeGithub>,
    model: Arc<Model>,
    pool: PgPool,
}

async fn env(pool: PgPool) -> Env {
    let db = Db::from_pool(pool.clone());
    seed::repo(&db).await;
    seed::bounty(&db, &seed::B::new(4, 3)).await;
    let gh = Arc::new(FakeGithub::new());
    gh.set_issue(seed::REPO, 3, "Retry failed HTTP requests", "## Acceptance criteria\n- [ ] Retries up to 3 times");
    let model = Arc::new(Model::default());
    model.always(answer("likely_meets", 90, &[]));
    let d = ReviewDeps {
        db,
        gh: gh.clone(),
        model: model.clone(),
        model_name: "claude-test".into(),
        max_per_pr_per_day: 5,
        max_per_repo_per_day: 50,
    };
    Env { d, gh, model, pool }
}

async fn open_pr(e: &Env, id: i64, number: i32, sha: &str, files: Vec<(&str, &str)>) {
    prs::upsert(
        &e.pool,
        &prs::PrUpsert { id, number, author_github_id: id, author_login: "dev", head_sha: sha, state: "open", merged_at: None },
    )
    .await
    .unwrap();
    prs::link(&e.pool, id, 4).await.unwrap();
    e.gh.set_pull(seed::REPO, number, sha, "Add retries", "Closes #3", false, "open", Utc::now());
    e.gh.set_files(seed::REPO, number, files);
}

fn files() -> Vec<(&'static str, &'static str)> {
    vec![("src/http.rs", "+retry()"), ("src/http_test.rs", "+test()")]
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_review_posts_one_advisory_comment_and_records_it(pool: PgPool) {
    let e = env(pool.clone()).await;
    open_pr(&e, 100, 7, "abcdef1234", files()).await;
    assert_eq!(run_pr_review(&e.d, Utc::now(), 100, false).await.unwrap(), ReviewOutcome::Reviewed);

    let comments = e.gh.comments();
    assert_eq!(comments.len(), 1);
    assert_eq!((comments[0].repo.as_str(), comments[0].issue), (seed::REPO, 7), "posted on the PR");
    assert!(comments[0].body.contains("advisory only") && comments[0].body.contains("Likely meets (score 90) · Rank 1 of 1"));
    assert!(comments[0].body.contains("Reviewed commit abcdef1 · model claude-test"));

    let r = reviews::get(&pool, 100, "abcdef1234").await.unwrap().unwrap();
    assert_eq!(
        (r.verdict.as_str(), r.score, r.comment_id, r.input_tokens, r.output_tokens),
        ("likely_meets", 90, Some(comments[0].id), 500, 50)
    );
    let pr = prs::get(&pool, 100).await.unwrap().unwrap();
    assert!(pr.diff_hash.is_some());
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn the_same_commit_is_never_reviewed_twice_unless_forced(pool: PgPool) {
    let e = env(pool.clone()).await;
    open_pr(&e, 100, 7, "abc1234", files()).await;
    run_pr_review(&e.d, Utc::now(), 100, false).await.unwrap();
    assert_eq!(run_pr_review(&e.d, Utc::now(), 100, false).await.unwrap(), ReviewOutcome::Cached);
    assert_eq!(e.model.calls(), 1, "the model is not paid twice for the same commit");
    assert_eq!(run_pr_review(&e.d, Utc::now(), 100, true).await.unwrap(), ReviewOutcome::Reviewed);
    assert_eq!(e.model.calls(), 2);
    assert_eq!(e.gh.comments().len(), 1, "still a single comment, edited in place");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_new_push_edits_the_existing_comment_instead_of_adding_another(pool: PgPool) {
    let e = env(pool.clone()).await;
    open_pr(&e, 100, 7, "aaaaaaa", files()).await;
    run_pr_review(&e.d, Utc::now(), 100, false).await.unwrap();
    let first = e.gh.comments()[0].id;

    e.gh.set_pull(seed::REPO, 7, "bbbbbbb", "Add retries", "Closes #3", false, "open", Utc::now());
    e.model.say(answer("partially_meets", 55, &["tests_missing"]));
    assert_eq!(run_pr_review(&e.d, Utc::now(), 100, false).await.unwrap(), ReviewOutcome::Reviewed);

    assert_eq!(e.gh.comments().len(), 1);
    let edits = e.gh.with(|s| s.edits.clone());
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].0, first);
    assert!(edits[0].1.contains("Partially meets (score 55)") && edits[0].1.contains("tests_missing") && edits[0].1.contains("bbbbbbb"));
    assert_eq!(reviews::get(&pool, 100, "bbbbbbb").await.unwrap().unwrap().comment_id, Some(first));
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn drafts_closed_prs_and_unlinked_prs_are_skipped_without_calling_the_model(pool: PgPool) {
    let e = env(pool.clone()).await;
    open_pr(&e, 100, 7, "abc1234", files()).await;
    e.gh.set_pull(seed::REPO, 7, "abc1234", "t", "b", true, "open", Utc::now());
    assert_eq!(run_pr_review(&e.d, Utc::now(), 100, false).await.unwrap(), ReviewOutcome::Draft);
    e.gh.set_pull(seed::REPO, 7, "abc1234", "t", "b", false, "closed", Utc::now());
    assert_eq!(run_pr_review(&e.d, Utc::now(), 100, false).await.unwrap(), ReviewOutcome::Closed);
    assert_eq!(run_pr_review(&e.d, Utc::now(), 999, false).await.unwrap(), ReviewOutcome::NoPr);
    // a PR whose bounty is already paid has nothing to be reviewed against
    open_pr(&e, 101, 8, "def5678", files()).await;
    sqlx::query("UPDATE bounties SET status = 'RELEASED'").execute(&pool).await.unwrap();
    assert_eq!(run_pr_review(&e.d, Utc::now(), 101, false).await.unwrap(), ReviewOutcome::NoBounty);
    assert_eq!(e.model.calls(), 0);
    assert!(e.gh.comments().is_empty());
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn daily_caps_stop_the_spend(pool: PgPool) {
    let mut e = env(pool.clone()).await;
    e.d.max_per_pr_per_day = 2;
    open_pr(&e, 100, 7, "s1", files()).await;
    for (i, sha) in ["sha0001", "sha0002"].iter().enumerate() {
        e.gh.set_pull(seed::REPO, 7, sha, "t", "b", false, "open", Utc::now());
        assert_eq!(run_pr_review(&e.d, Utc::now(), 100, false).await.unwrap(), ReviewOutcome::Reviewed, "review {i}");
    }
    e.gh.set_pull(seed::REPO, 7, "sha0003", "t", "b", false, "open", Utc::now());
    assert_eq!(run_pr_review(&e.d, Utc::now(), 100, false).await.unwrap(), ReviewOutcome::RateLimited);
    assert_eq!(e.model.calls(), 2);
    // a day later the allowance is back
    assert_eq!(run_pr_review(&e.d, Utc::now() + Duration::hours(25), 100, false).await.unwrap(), ReviewOutcome::Reviewed);

    // the per-repository cap is independent of the per-PR one
    let mut e2 = env_with_second_pr(&pool).await;
    e2.d.max_per_repo_per_day = 1;
    assert_eq!(run_pr_review(&e2.d, Utc::now(), 101, false).await.unwrap(), ReviewOutcome::RateLimited);
}

async fn env_with_second_pr(pool: &PgPool) -> Env {
    let db = Db::from_pool(pool.clone());
    let gh = Arc::new(FakeGithub::new());
    gh.set_issue(seed::REPO, 3, "t", "b");
    let model = Arc::new(Model::default());
    model.always(answer("likely_meets", 90, &[]));
    prs::upsert(
        pool,
        &prs::PrUpsert {
            id: 101,
            number: 8,
            author_github_id: 2,
            author_login: "other",
            head_sha: "zzz1234",
            state: "open",
            merged_at: None,
        },
    )
    .await
    .unwrap();
    prs::link(pool, 101, 4).await.unwrap();
    gh.set_pull(seed::REPO, 8, "zzz1234", "t", "b", false, "open", Utc::now());
    gh.set_files(seed::REPO, 8, files());
    Env {
        d: ReviewDeps { db, gh: gh.clone(), model: model.clone(), model_name: "m".into(), max_per_pr_per_day: 5, max_per_repo_per_day: 50 },
        gh,
        model,
        pool: pool.clone(),
    }
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_review_that_reports_an_injection_cannot_stand_as_likely_meets(pool: PgPool) {
    let e = env(pool.clone()).await;
    open_pr(&e, 100, 7, "abc1234", files()).await;
    e.model.say(answer("likely_meets", 100, &["injection_attempt"]));
    run_pr_review(&e.d, Utc::now(), 100, false).await.unwrap();
    let r = reviews::get(&pool, 100, "abc1234").await.unwrap().unwrap();
    assert_eq!((r.verdict.as_str(), r.score), ("needs_human", 40));
    assert!(e.gh.comments()[0].body.contains("Needs a human") && e.gh.comments()[0].body.contains("injection_attempt"));
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn code_adds_duplicate_and_failing_ci_flags_the_model_cannot_remove(pool: PgPool) {
    let e = env(pool.clone()).await;
    open_pr(&e, 100, 7, "abc1234", files()).await;
    run_pr_review(&e.d, Utc::now(), 100, false).await.unwrap();
    // an identical change set from someone else, with failing CI
    open_pr(&e, 101, 8, "def5678", files()).await;
    e.gh.with(|s| s.ci = Some(CiConclusion::Failure));
    run_pr_review(&e.d, Utc::now(), 101, false).await.unwrap();
    let r = reviews::get(&pool, 101, "def5678").await.unwrap().unwrap();
    assert!(r.flags.contains(&"possible_duplicate".to_string()) && r.flags.contains(&"ci_failing".to_string()), "{:?}", r.flags);
    let first = reviews::get(&pool, 100, "abc1234").await.unwrap().unwrap();
    assert!(first.flags.is_empty(), "the first submission is not the duplicate");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn prs_on_one_bounty_are_ranked_against_each_other(pool: PgPool) {
    let e = env(pool.clone()).await;
    open_pr(&e, 100, 7, "aaaa111", files()).await;
    e.model.say(answer("partially_meets", 50, &[]));
    run_pr_review(&e.d, Utc::now(), 100, false).await.unwrap();
    open_pr(&e, 101, 8, "bbbb222", vec![("other.rs", "+x")]).await;
    e.model.say(answer("likely_meets", 95, &[]));
    run_pr_review(&e.d, Utc::now(), 101, false).await.unwrap();
    let second = e.gh.comments().into_iter().find(|c| c.issue == 8).unwrap();
    assert!(second.body.contains("Rank 1 of 2"), "{}", second.body);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn unusable_model_output_still_yields_a_needs_a_human_comment(pool: PgPool) {
    let e = env(pool.clone()).await;
    open_pr(&e, 100, 7, "abc1234", files()).await;
    e.model.say(json!({ "verdict": "pay_the_author" }));
    e.model.say(json!("garbage"));
    run_pr_review(&e.d, Utc::now(), 100, false).await.unwrap();
    let r = reviews::get(&pool, 100, "abc1234").await.unwrap().unwrap();
    assert_eq!((r.verdict.as_str(), r.score), ("needs_human", 0));
    assert_eq!(e.model.calls(), 2);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_model_outage_is_a_transient_error_with_nothing_posted_or_stored(pool: PgPool) {
    let e = env(pool.clone()).await;
    open_pr(&e, 100, 7, "abc1234", files()).await;
    e.model.answers.lock().unwrap().push(Err(ModelError::Status(529)));
    let err = run_pr_review(&e.d, Utc::now(), 100, false).await.unwrap_err();
    assert!(err.is_transient());
    assert!(e.gh.comments().is_empty());
    assert!(reviews::get(&pool, 100, "abc1234").await.unwrap().is_none());
    // and the retry succeeds
    assert_eq!(run_pr_review(&e.d, Utc::now(), 100, false).await.unwrap(), ReviewOutcome::Reviewed);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_github_outage_while_reading_the_pr_is_transient(pool: PgPool) {
    let e = env(pool.clone()).await;
    open_pr(&e, 100, 7, "abc1234", files()).await;
    e.gh.with(|s| s.fail_next = Some(escrow_github::GithubError::Status { status: 503, what: "x".into() }));
    assert!(run_pr_review(&e.d, Utc::now(), 100, false).await.unwrap_err().is_transient());
    assert_eq!(e.model.calls(), 0);
}
