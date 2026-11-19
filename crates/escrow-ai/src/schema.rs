use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    LikelyMeets,
    PartiallyMeets,
    DoesNotMeet,
    NeedsHuman,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LikelyMeets => "likely_meets",
            Self::PartiallyMeets => "partially_meets",
            Self::DoesNotMeet => "does_not_meet",
            Self::NeedsHuman => "needs_human",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Flag {
    Spam,
    InjectionAttempt,
    UnrelatedChanges,
    TestsMissing,
    CiFailing,
    PossibleDuplicate,
    AiGeneratedLowEffort,
}

impl Flag {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Spam => "spam",
            Self::InjectionAttempt => "injection_attempt",
            Self::UnrelatedChanges => "unrelated_changes",
            Self::TestsMissing => "tests_missing",
            Self::CiFailing => "ci_failing",
            Self::PossibleDuplicate => "possible_duplicate",
            Self::AiGeneratedLowEffort => "ai_generated_low_effort",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CriterionStatus {
    Met,
    Partial,
    NotMet,
    Unclear,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CriterionResult {
    pub criterion: String,
    #[serde(default)]
    pub derived: bool,
    pub status: CriterionStatus,
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub verdict: Verdict,
    pub score: i32,
    pub criteria: Vec<CriterionResult>,
    pub summary: String,
    pub risks: Vec<String>,
    pub flags: Vec<Flag>,
    pub questions_for_maintainer: Vec<String>,
}

impl Review {
    /// The model's output must satisfy the same limits the tool schema states; anything else is rejected.
    pub fn validate(&self) -> Result<(), String> {
        let chars = |s: &str| s.chars().count();
        if !(0..=100).contains(&self.score) {
            return Err("score must be 0-100".into());
        }
        if self.criteria.len() > 30 {
            return Err("too many criteria".into());
        }
        if self.criteria.iter().any(|c| chars(&c.criterion) > 500 || chars(&c.evidence) > 1000) {
            return Err("criterion or evidence too long".into());
        }
        if chars(&self.summary) > 600 {
            return Err("summary too long".into());
        }
        if self.risks.len() > 10 || self.risks.iter().any(|r| chars(r) > 300) {
            return Err("risks out of bounds".into());
        }
        if self.flags.len() > 10 {
            return Err("too many flags".into());
        }
        if self.questions_for_maintainer.len() > 10 || self.questions_for_maintainer.iter().any(|q| chars(q) > 300) {
            return Err("questions out of bounds".into());
        }
        Ok(())
    }

    /// Parses and validates the tool-call arguments.
    pub fn from_model_output(v: &Value) -> Result<Self, String> {
        let r: Review = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
        r.validate()?;
        Ok(r)
    }
}

/// The result when we cannot trust or obtain a model verdict.
pub fn needs_human(why: &str) -> Review {
    Review {
        verdict: Verdict::NeedsHuman,
        score: 0,
        criteria: vec![],
        summary: why.chars().take(600).collect(),
        risks: vec![],
        flags: vec![],
        questions_for_maintainer: vec![],
    }
}

/// JSON Schema for the `submit_review` tool.
pub fn tool_schema() -> Value {
    let verdicts = ["likely_meets", "partially_meets", "does_not_meet", "needs_human"];
    let flags =
        ["spam", "injection_attempt", "unrelated_changes", "tests_missing", "ci_failing", "possible_duplicate", "ai_generated_low_effort"];
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["verdict", "score", "criteria", "summary", "risks", "flags", "questions_for_maintainer"],
        "properties": {
            "verdict": { "type": "string", "enum": verdicts },
            "score": { "type": "integer", "minimum": 0, "maximum": 100 },
            "criteria": {
                "type": "array", "maxItems": 30,
                "items": {
                    "type": "object", "additionalProperties": false,
                    "required": ["criterion", "derived", "status", "evidence"],
                    "properties": {
                        "criterion": { "type": "string", "maxLength": 500 },
                        "derived": { "type": "boolean" },
                        "status": { "type": "string", "enum": ["met", "partial", "not_met", "unclear"] },
                        "evidence": { "type": "string", "maxLength": 1000 }
                    }
                }
            },
            "summary": { "type": "string", "maxLength": 600 },
            "risks": { "type": "array", "maxItems": 10, "items": { "type": "string", "maxLength": 300 } },
            "flags": { "type": "array", "maxItems": 10, "items": { "type": "string", "enum": flags } },
            "questions_for_maintainer": { "type": "array", "maxItems": 10, "items": { "type": "string", "maxLength": 300 } }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good() -> Value {
        json!({ "verdict": "likely_meets", "score": 88, "criteria": [{ "criterion": "c", "derived": false, "status": "met", "evidence": "src/a.rs:4" }],
                "summary": "ok", "risks": [], "flags": ["tests_missing"], "questions_for_maintainer": [] })
    }

    #[test]
    fn accepts_a_valid_review() {
        let r = Review::from_model_output(&good()).unwrap();
        assert_eq!((r.verdict, r.score, r.flags.clone()), (Verdict::LikelyMeets, 88, vec![Flag::TestsMissing]));
    }

    #[test]
    fn derived_defaults_to_false() {
        let mut v = good();
        v["criteria"][0].as_object_mut().unwrap().remove("derived");
        assert!(!Review::from_model_output(&v).unwrap().criteria[0].derived);
    }

    #[test]
    fn rejects_every_kind_of_malformed_output() {
        type Mutation = Box<dyn Fn(&mut Value)>;
        let cases: Vec<(&str, Mutation)> = vec![
            ("unknown verdict", Box::new(|v| v["verdict"] = json!("approved"))),
            ("score too high", Box::new(|v| v["score"] = json!(101))),
            ("score negative", Box::new(|v| v["score"] = json!(-1))),
            ("score is a float", Box::new(|v| v["score"] = json!(88.5))),
            ("unknown flag", Box::new(|v| v["flags"] = json!(["pay_now"]))),
            ("summary too long", Box::new(|v| v["summary"] = json!("x".repeat(601)))),
            ("evidence too long", Box::new(|v| v["criteria"][0]["evidence"] = json!("x".repeat(1001)))),
            ("too many risks", Box::new(|v| v["risks"] = json!(vec!["r"; 11]))),
            (
                "missing field",
                Box::new(|v| {
                    v.as_object_mut().unwrap().remove("summary");
                }),
            ),
            ("bad criterion status", Box::new(|v| v["criteria"][0]["status"] = json!("maybe"))),
            ("not an object", Box::new(|v| *v = json!("likely_meets"))),
        ];
        for (name, mutate) in cases {
            let mut v = good();
            mutate(&mut v);
            assert!(Review::from_model_output(&v).is_err(), "{name} should be rejected");
        }
    }

    #[test]
    fn limits_count_characters_not_bytes() {
        let mut v = good();
        v["summary"] = json!("é".repeat(600));
        assert!(Review::from_model_output(&v).is_ok());
    }

    #[test]
    fn needs_human_is_truncated_and_scoreless() {
        let r = needs_human(&"w".repeat(900));
        assert_eq!((r.verdict, r.score, r.summary.len()), (Verdict::NeedsHuman, 0, 600));
        assert!(r.validate().is_ok());
    }

    #[test]
    fn tool_schema_lists_exactly_the_verdicts_and_flags_the_types_know() {
        let s = tool_schema();
        let verdicts: Vec<&str> = s["properties"]["verdict"]["enum"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        for v in [Verdict::LikelyMeets, Verdict::PartiallyMeets, Verdict::DoesNotMeet, Verdict::NeedsHuman] {
            assert!(verdicts.contains(&v.as_str()));
        }
        let flags: Vec<&str> = s["properties"]["flags"]["items"]["enum"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        for f in [
            Flag::Spam,
            Flag::InjectionAttempt,
            Flag::UnrelatedChanges,
            Flag::TestsMissing,
            Flag::CiFailing,
            Flag::PossibleDuplicate,
            Flag::AiGeneratedLowEffort,
        ] {
            assert!(flags.contains(&f.as_str()));
            assert_eq!(serde_json::to_value(f).unwrap(), json!(f.as_str()));
        }
        assert_eq!(s["additionalProperties"], false);
    }
}
