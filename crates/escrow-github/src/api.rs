use crate::error::GithubError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Issue {
    pub number: i32,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    pub html_url: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PullUser {
    pub id: i64,
    pub login: String,
    #[serde(rename = "type", default)]
    pub kind: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Head {
    pub sha: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Pull {
    pub number: i32,
    pub title: String,
    pub body: Option<String>,
    #[serde(default)]
    pub draft: bool,
    pub state: String,
    #[serde(default)]
    pub merged: bool,
    pub head: Head,
    pub user: PullUser,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PullFile {
    pub filename: String,
    #[serde(default)]
    pub additions: u32,
    #[serde(default)]
    pub deletions: u32,
    pub patch: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiConclusion {
    Success,
    Failure,
    Pending,
    None,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct InstallationInfo {
    pub id: i64,
    pub account: AccountInfo,
    pub suspended_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct AccountInfo {
    pub login: String,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RepoInfo {
    pub id: i64,
    pub full_name: String,
    #[serde(default)]
    pub private: bool,
}

/// What the backend does on GitHub. Production: [`crate::GithubClient`]; tests: in-memory fakes.
#[async_trait]
pub trait GithubApi: Send + Sync {
    /// Repository permission of a user: `admin | maintain | write | triage | read | none`.
    async fn get_permission(&self, installation: i64, repo: &str, login: &str) -> Result<String, GithubError>;
    async fn post_comment(&self, installation: i64, repo: &str, issue: i32, body: &str) -> Result<i64, GithubError>;
    async fn update_comment(&self, installation: i64, repo: &str, comment_id: i64, body: &str) -> Result<(), GithubError>;
    async fn get_issue(&self, installation: i64, repo: &str, issue: i32) -> Result<Issue, GithubError>;
    /// Creates the label if needed and sets it on the issue, replacing earlier `bounty:` labels.
    async fn set_bounty_label(&self, installation: i64, repo: &str, issue: i32, label: &str) -> Result<(), GithubError>;
    async fn get_pull(&self, installation: i64, repo: &str, pr: i32) -> Result<Pull, GithubError>;
    async fn get_pull_files(&self, installation: i64, repo: &str, pr: i32) -> Result<Vec<PullFile>, GithubError>;
    async fn get_ci_conclusion(&self, installation: i64, repo: &str, sha: &str) -> Result<CiConclusion, GithubError>;
    async fn get_readme_excerpt(&self, installation: i64, repo: &str, max_chars: usize) -> Result<String, GithubError>;
    /// All installations of this App (authenticated as the App itself).
    async fn list_installations(&self) -> Result<Vec<InstallationInfo>, GithubError>;
    async fn list_repositories(&self, installation: i64) -> Result<Vec<RepoInfo>, GithubError>;
}
