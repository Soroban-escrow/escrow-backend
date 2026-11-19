//! Running a review, ranking PRs against each other, and rendering the single comment per PR.

use crate::{
    input::{FileChange, ReviewInput, SYSTEM_PROMPT, build_user_prompt},
    schema::{Flag, Review, Verdict, needs_human},
};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("model request failed with status {0}")]
    Status(u16),
    #[error("model request failed: {0}")]
    Network(String),
    #[error("model answered without the expected tool call")]
    NoToolCall,
}

impl ModelError {
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Network(_)) || matches!(self, Self::Status(s) if *s == 429 || *s >= 500)
    }
}

pub struct ModelOutput {
    /// the tool-call arguments, as the model produced them (not yet trusted)
    pub output: serde_json::Value,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// The model call. Production: [`crate::AnthropicModel`]; tests: scripted fakes.
#[async_trait]
pub trait ModelPort: Send + Sync {
    async fn complete(&self, system: &str, user: &str) -> Result<ModelOutput, ModelError>;
}

pub struct Reviewed {
    pub review: Review,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// Asks the model, validating its answer against the schema. One retry on a malformed answer; if it is
/// malformed twice the result is "needs a human", never a guess. Transport errors are returned so the job retries.
pub async fn review_pr(m: &dyn ModelPort, input: &ReviewInput) -> Result<Reviewed, ModelError> {
    let prompt = build_user_prompt(input).text;
    let (mut inp, mut out) = (0, 0);
    for _ in 0..2 {
        let r = m.complete(SYSTEM_PROMPT, &prompt).await?;
        inp += r.input_tokens;
        out += r.output_tokens;
        if let Ok(review) = Review::from_model_output(&r.output) {
            return Ok(Reviewed { review, input_tokens: inp, output_tokens: out });
        }
    }
    Ok(Reviewed {
        review: needs_human("The model returned invalid output twice; a human should review this PR."),
        input_tokens: inp,
        output_tokens: out,
    })
}

/// Defence in depth on top of the prompt: a review that reports an injection attempt can never stand as
/// "likely meets", whatever the model said, and its score is capped. (It is advisory either way.)
pub fn sanitize_review(mut r: Review, extra_flags: &[Flag]) -> Review {
    for f in extra_flags {
        if !r.flags.contains(f) {
            r.flags.push(*f);
        }
    }
    if r.flags.contains(&Flag::InjectionAttempt) {
        if r.verdict == Verdict::LikelyMeets {
            r.verdict = Verdict::NeedsHuman;
        }
        r.score = r.score.min(40);
    }
    r
}

/// Same changed content (ignoring whitespace) hashes equal: duplicate detection across PRs on one bounty.
pub fn diff_hash(files: &[FileChange]) -> String {
    let mut sorted: Vec<&FileChange> = files.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let mut h = Sha256::new();
    for f in sorted {
        h.update(f.path.as_bytes());
        h.update([0]);
        let squeezed: String = f.patch.as_deref().unwrap_or("").split_whitespace().collect();
        h.update(squeezed.as_bytes());
        h.update([0]);
    }
    hex::encode(h.finalize())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ci {
    Success,
    Pending,
    None,
    Failure,
}

#[derive(Debug, Clone)]
pub struct Rankable {
    pub id: i64,
    pub verdict: Verdict,
    pub score: i32,
    pub ci: Ci,
    pub submitted_at: i64,
}

/// Verdict, then score, then CI, then earliest submission. Returns ids, best first.
pub fn rank_prs(prs: &[Rankable]) -> Vec<i64> {
    let vrank = |v: Verdict| match v {
        Verdict::LikelyMeets => 0,
        Verdict::PartiallyMeets => 1,
        Verdict::DoesNotMeet => 2,
        Verdict::NeedsHuman => 3,
    };
    let crank = |c: Ci| match c {
        Ci::Success => 0,
        Ci::Pending | Ci::None => 1,
        Ci::Failure => 2,
    };
    let mut v = prs.to_vec();
    v.sort_by_key(|p| (vrank(p.verdict), -p.score, crank(p.ci), p.submitted_at));
    v.into_iter().map(|p| p.id).collect()
}

fn cell(s: &str) -> String {
    escrow_core::card::esc(s)
}

pub fn render_review(r: &Review, rank: Option<(usize, usize)>, sha: &str, model: &str) -> String {
    let words = match r.verdict {
        Verdict::LikelyMeets => "Likely meets",
        Verdict::PartiallyMeets => "Partially meets",
        Verdict::DoesNotMeet => "Does not meet",
        Verdict::NeedsHuman => "Needs a human",
    };
    let icon = |s| match s {
        crate::schema::CriterionStatus::Met => "✅",
        crate::schema::CriterionStatus::Partial => "⚠️",
        crate::schema::CriterionStatus::NotMet => "❌",
        crate::schema::CriterionStatus::Unclear => "❔",
    };
    let mut parts = vec![
        "🤖 **SkillEscrow review** (advisory only, the maintainer decides)".to_string(),
        format!(
            "**Verdict:** {words} (score {}){}",
            r.score,
            rank.map(|(n, of)| format!(" · Rank {n} of {of} on this bounty")).unwrap_or_default()
        ),
        cell(&r.summary),
    ];
    parts.extend(r.criteria.iter().map(|c| {
        format!("{} {}{}: {}", icon(c.status), cell(&c.criterion), if c.derived { " _(derived)_" } else { "" }, cell(&c.evidence))
    }));
    if !r.risks.is_empty() {
        parts.push(format!("**Risks:** {}", r.risks.iter().map(|x| cell(x)).collect::<Vec<_>>().join("; ")));
    }
    if !r.flags.is_empty() {
        parts.push(format!("**Flags:** {}", r.flags.iter().map(|f| f.as_str()).collect::<Vec<_>>().join(", ")));
    }
    parts.push(format!("_Reviewed commit {} · model {}_", &sha[..sha.len().min(7)], model));
    parts.retain(|p| !p.is_empty());
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    struct Script(Mutex<Vec<Result<ModelOutput, ModelError>>>, Mutex<usize>);
    #[async_trait]
    impl ModelPort for Script {
        async fn complete(&self, _s: &str, _u: &str) -> Result<ModelOutput, ModelError> {
            *self.1.lock().unwrap() += 1;
            self.0.lock().unwrap().remove(0)
        }
    }
    fn out(v: serde_json::Value) -> Result<ModelOutput, ModelError> {
        Ok(ModelOutput { output: v, input_tokens: 100, output_tokens: 10 })
    }
    fn good() -> serde_json::Value {
        json!({ "verdict": "likely_meets", "score": 90, "criteria": [], "summary": "fine", "risks": [], "flags": [], "questions_for_maintainer": [] })
    }
    fn review(v: Verdict, score: i32, flags: Vec<Flag>) -> Review {
        Review { verdict: v, score, criteria: vec![], summary: "s".into(), risks: vec![], flags, questions_for_maintainer: vec![] }
    }

    #[tokio::test]
    async fn a_valid_answer_is_used_and_tokens_counted() {
        let m = Script(Mutex::new(vec![out(good())]), Mutex::new(0));
        let r = review_pr(&m, &ReviewInput::default()).await.unwrap();
        assert_eq!((r.review.verdict, r.input_tokens, r.output_tokens), (Verdict::LikelyMeets, 100, 10));
    }

    #[tokio::test]
    async fn one_malformed_answer_is_retried() {
        let m = Script(Mutex::new(vec![out(json!({ "verdict": "approved" })), out(good())]), Mutex::new(0));
        let r = review_pr(&m, &ReviewInput::default()).await.unwrap();
        assert_eq!((r.review.verdict, r.input_tokens), (Verdict::LikelyMeets, 200), "tokens of both calls are counted");
    }

    #[tokio::test]
    async fn two_malformed_answers_mean_a_human_looks_it_never_a_guess() {
        let m = Script(Mutex::new(vec![out(json!({})), out(json!("likely_meets"))]), Mutex::new(0));
        let r = review_pr(&m, &ReviewInput::default()).await.unwrap();
        assert_eq!((r.review.verdict, r.review.score), (Verdict::NeedsHuman, 0));
        assert_eq!(*m.1.lock().unwrap(), 2, "exactly two attempts");
    }

    #[tokio::test]
    async fn transport_errors_propagate_so_the_job_retries() {
        let m = Script(Mutex::new(vec![Err(ModelError::Status(529))]), Mutex::new(0));
        let e = review_pr(&m, &ReviewInput::default()).await.err().unwrap();
        assert!(e.is_transient());
        assert!(!ModelError::Status(400).is_transient());
        assert!(!ModelError::NoToolCall.is_transient());
    }

    #[test]
    fn an_injection_flag_downgrades_likely_meets_and_caps_the_score() {
        let r = sanitize_review(review(Verdict::LikelyMeets, 100, vec![Flag::InjectionAttempt]), &[]);
        assert_eq!((r.verdict, r.score), (Verdict::NeedsHuman, 40));
        let r = sanitize_review(review(Verdict::DoesNotMeet, 20, vec![Flag::InjectionAttempt]), &[]);
        assert_eq!((r.verdict, r.score), (Verdict::DoesNotMeet, 20), "an already-low review is left alone");
        let clean = sanitize_review(review(Verdict::LikelyMeets, 95, vec![]), &[Flag::CiFailing]);
        assert_eq!((clean.verdict, clean.score, clean.flags), (Verdict::LikelyMeets, 95, vec![Flag::CiFailing]));
    }

    #[test]
    fn extra_flags_added_by_code_are_not_duplicated() {
        let r = sanitize_review(review(Verdict::PartiallyMeets, 50, vec![Flag::CiFailing]), &[Flag::CiFailing, Flag::PossibleDuplicate]);
        assert_eq!(r.flags, vec![Flag::CiFailing, Flag::PossibleDuplicate]);
    }

    fn f(path: &str, patch: &str) -> FileChange {
        FileChange { path: path.into(), additions: 1, deletions: 0, patch: Some(patch.into()) }
    }

    #[test]
    fn diff_hash_ignores_whitespace_and_file_order_but_not_content() {
        let a = diff_hash(&[f("a.rs", "+let x = 1;"), f("b.rs", "+y")]);
        assert_eq!(a, diff_hash(&[f("b.rs", "+y"), f("a.rs", "+let   x = 1;\n")]));
        assert_ne!(a, diff_hash(&[f("a.rs", "+let x = 2;"), f("b.rs", "+y")]));
        assert_ne!(a, diff_hash(&[f("a.rs", "+let x = 1;"), f("c.rs", "+y")]), "the path matters");
    }

    #[test]
    fn ranking_orders_by_verdict_then_score_then_ci_then_age() {
        let p = |id, verdict, score, ci, at| Rankable { id, verdict, score, ci, submitted_at: at };
        let ranked = rank_prs(&[
            p(1, Verdict::PartiallyMeets, 80, Ci::Success, 1),
            p(2, Verdict::LikelyMeets, 70, Ci::Failure, 2),
            p(3, Verdict::LikelyMeets, 70, Ci::Success, 3),
            p(4, Verdict::LikelyMeets, 90, Ci::None, 9),
            p(5, Verdict::NeedsHuman, 99, Ci::Success, 0),
            p(6, Verdict::LikelyMeets, 70, Ci::Success, 1),
        ]);
        assert_eq!(ranked, [4, 6, 3, 2, 1, 5]);
    }

    #[test]
    fn the_comment_escapes_model_text_and_states_what_it_is() {
        let mut r = review(Verdict::PartiallyMeets, 60, vec![Flag::TestsMissing]);
        r.summary = "see [this](http://evil) @maintainer".into();
        r.criteria = vec![crate::schema::CriterionResult {
            criterion: "adds `tests`".into(),
            derived: true,
            status: crate::schema::CriterionStatus::NotMet,
            evidence: "none".into(),
        }];
        let c = render_review(&r, Some((2, 3)), "abcdef0123", "claude-test");
        assert!(c.contains("advisory only"));
        assert!(c.contains("**Verdict:** Partially meets (score 60) · Rank 2 of 3 on this bounty"));
        assert!(!c.contains("[this](http") && c.contains("\\@maintainer"));
        assert!(c.contains("❌ adds \\`tests\\` _(derived)_: none"));
        assert!(c.contains("**Flags:** tests_missing"));
        assert!(c.ends_with("_Reviewed commit abcdef0 · model claude-test_"));
    }
}
