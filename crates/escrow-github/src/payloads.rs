//! Typed webhook payloads. Only the fields we use; GitHub adds fields freely, so nothing denies unknowns.

use chrono::{DateTime, Utc};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Account {
    pub login: String,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InstallationRef {
    pub id: i64,
    pub account: Option<Account>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RepoRef {
    pub id: i64,
    pub full_name: String,
    #[serde(default)]
    pub private: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RepoId {
    pub id: i64,
    #[serde(default)]
    pub full_name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InstallationEvent {
    pub action: String,
    pub installation: InstallationRef,
    #[serde(default)]
    pub repositories: Vec<RepoRef>,
    #[serde(default)]
    pub repositories_added: Vec<RepoRef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Actor {
    pub id: i64,
    pub login: String,
    #[serde(rename = "type", default)]
    pub kind: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IssueRef {
    pub number: i32,
    #[serde(default)]
    pub state: String,
    /// present (and non-null) only when the "issue" is really a pull request
    #[serde(default)]
    pub pull_request: Option<serde_json::Value>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommentRef {
    pub id: i64,
    pub body: String,
    pub user: Actor,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IssueCommentEvent {
    pub action: String,
    pub installation: Option<InstallationRef>,
    pub repository: RepoId,
    pub issue: IssueRef,
    pub comment: CommentRef,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IssuesEvent {
    pub action: String,
    pub repository: RepoId,
    pub issue: IssueRef,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HeadRef {
    pub sha: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PullRef {
    pub id: i64,
    pub number: i32,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub merged: bool,
    pub merged_at: Option<DateTime<Utc>>,
    pub state: String,
    pub head: HeadRef,
    pub user: Actor,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PullRequestEvent {
    pub action: String,
    pub repository: RepoId,
    pub pull_request: PullRef,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn issue_comment_payload_parses_with_extra_fields() {
        let e: IssueCommentEvent = serde_json::from_value(json!({
            "action": "created", "sender": {"id": 1}, "extra": [1, 2],
            "installation": {"id": 99},
            "repository": {"id": 5, "full_name": "acme/widgets", "private": false, "owner": {}},
            "issue": {"number": 3, "state": "open", "labels": []},
            "comment": {"id": 11, "body": "/bounty 5 XLM", "user": {"id": 7, "login": "alice", "type": "User"}}
        }))
        .unwrap();
        assert_eq!((e.installation.unwrap().id, e.issue.number, e.comment.user.kind.as_str()), (99, 3, "User"));
        assert!(e.issue.pull_request.is_none());
    }

    #[test]
    fn a_pr_conversation_comment_is_recognised_by_the_pull_request_key() {
        let e: IssueCommentEvent = serde_json::from_value(json!({
            "action": "created", "repository": {"id": 5, "full_name": "a/b"},
            "issue": {"number": 3, "state": "open", "pull_request": {"url": "u"}},
            "comment": {"id": 1, "body": "x", "user": {"id": 1, "login": "a"}}
        }))
        .unwrap();
        assert!(e.issue.pull_request.is_some());
        let n: IssueCommentEvent = serde_json::from_value(json!({
            "action": "created", "repository": {"id": 5}, "issue": {"number": 3, "pull_request": null},
            "comment": {"id": 1, "body": "x", "user": {"id": 1, "login": "a"}}
        }))
        .unwrap();
        assert!(n.issue.pull_request.is_none(), "an explicit null is not a pull request");
    }

    #[test]
    fn pull_request_and_installation_payloads_parse() {
        let p: PullRequestEvent = serde_json::from_value(json!({
            "action": "closed", "repository": {"id": 5, "full_name": "a/b"},
            "pull_request": {"id": 77, "number": 8, "body": "Closes #3", "merged": true, "merged_at": "2026-11-03T10:00:00Z",
                             "state": "closed", "head": {"sha": "abc"}, "user": {"id": 2, "login": "bob", "type": "User"}}
        }))
        .unwrap();
        assert!(p.pull_request.merged && p.pull_request.merged_at.is_some() && !p.pull_request.draft);
        let i: InstallationEvent = serde_json::from_value(json!({
            "action": "created", "installation": {"id": 1, "account": {"login": "acme", "type": "Organization"}},
            "repositories": [{"id": 5, "full_name": "acme/widgets", "private": true}]
        }))
        .unwrap();
        assert!(i.repositories[0].private && i.repositories_added.is_empty());
        assert_eq!(i.installation.account.unwrap().login, "acme");
    }

    #[test]
    fn missing_required_fields_are_errors() {
        assert!(serde_json::from_value::<PullRequestEvent>(json!({"action": "closed"})).is_err());
    }
}
