use async_trait::async_trait;
use chrono::{DateTime, Utc};
use escrow_github::{CiConclusion, GithubApi, GithubError, InstallationInfo, Issue, Pull, PullFile, RepoInfo};
use std::{collections::HashMap, sync::Mutex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Posted {
    pub id: i64,
    pub repo: String,
    pub issue: i32,
    pub body: String,
}

#[derive(Default)]
pub struct GhState {
    next_id: i64,
    pub comments: Vec<Posted>,
    pub edits: Vec<(i64, String)>,
    pub labels: Vec<(String, i32, String)>,
    pub permissions: HashMap<String, String>,
    pub issues: HashMap<(String, i32), Issue>,
    pub pulls: HashMap<(String, i32), Pull>,
    pub files: HashMap<(String, i32), Vec<PullFile>>,
    pub ci: Option<CiConclusion>,
    pub installations: Vec<InstallationInfo>,
    pub repos: Vec<RepoInfo>,
    /// the next call (of any kind) fails with this
    pub fail_next: Option<GithubError>,
    /// every call is recorded here by name
    pub calls: Vec<String>,
    pub delay_ms: u64,
}

/// A scriptable GitHub. Unknown users have permission "none"; comments get sequential ids starting at 1000.
#[derive(Default)]
pub struct FakeGithub {
    pub s: Mutex<GhState>,
}

impl FakeGithub {
    pub fn new() -> Self {
        let g = Self::default();
        g.s.lock().unwrap().next_id = 1000;
        g
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut GhState) -> R) -> R {
        f(&mut self.s.lock().unwrap())
    }

    pub fn allow(&self, login: &str, permission: &str) {
        self.with(|s| s.permissions.insert(login.to_string(), permission.to_string()));
    }

    pub fn comments(&self) -> Vec<Posted> {
        self.with(|s| s.comments.clone())
    }

    pub fn bodies(&self) -> Vec<String> {
        self.with(|s| s.comments.iter().map(|c| c.body.clone()).collect())
    }

    pub fn calls(&self) -> Vec<String> {
        self.with(|s| s.calls.clone())
    }

    pub fn set_issue(&self, repo: &str, number: i32, title: &str, body: &str) {
        self.with(|s| {
            s.issues.insert(
                (repo.into(), number),
                Issue {
                    number,
                    title: title.into(),
                    body: Some(body.into()),
                    state: "open".into(),
                    html_url: format!("https://github.com/{repo}/issues/{number}"),
                },
            );
        });
    }

    #[allow(clippy::too_many_arguments)] // a flat fixture helper: every test names the same few facts
    pub fn set_pull(&self, repo: &str, number: i32, sha: &str, title: &str, body: &str, draft: bool, state: &str, created: DateTime<Utc>) {
        self.with(|s| {
            s.pulls.insert(
                (repo.into(), number),
                Pull {
                    number,
                    title: title.into(),
                    body: Some(body.into()),
                    draft,
                    state: state.into(),
                    merged: false,
                    head: escrow_github::api::Head { sha: sha.into() },
                    user: escrow_github::PullUser { id: 1, login: "dev".into(), kind: "User".into() },
                    created_at: created,
                },
            );
        });
    }

    pub fn set_files(&self, repo: &str, pr: i32, files: Vec<(&str, &str)>) {
        self.with(|s| {
            s.files.insert(
                (repo.into(), pr),
                files
                    .into_iter()
                    .map(|(f, p)| PullFile { filename: f.into(), additions: 1, deletions: 0, patch: Some(p.into()) })
                    .collect(),
            );
        });
    }

    async fn enter(&self, name: &str) -> Result<(), GithubError> {
        let (delay, fail) = self.with(|s| {
            s.calls.push(name.to_string());
            (s.delay_ms, s.fail_next.take())
        });
        if delay > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }
        fail.map_or(Ok(()), Err)
    }
}

#[async_trait]
impl GithubApi for FakeGithub {
    async fn get_permission(&self, _i: i64, _repo: &str, login: &str) -> Result<String, GithubError> {
        self.enter("get_permission").await?;
        Ok(self.with(|s| s.permissions.get(login).cloned().unwrap_or_else(|| "none".into())))
    }

    async fn post_comment(&self, _i: i64, repo: &str, issue: i32, body: &str) -> Result<i64, GithubError> {
        self.enter("post_comment").await?;
        Ok(self.with(|s| {
            s.next_id += 1;
            let id = s.next_id;
            s.comments.push(Posted { id, repo: repo.into(), issue, body: body.into() });
            id
        }))
    }

    async fn update_comment(&self, _i: i64, _repo: &str, comment_id: i64, body: &str) -> Result<(), GithubError> {
        self.enter("update_comment").await?;
        self.with(|s| {
            s.edits.push((comment_id, body.into()));
            if let Some(c) = s.comments.iter_mut().find(|c| c.id == comment_id) {
                c.body = body.into();
            }
        });
        Ok(())
    }

    async fn get_issue(&self, _i: i64, repo: &str, issue: i32) -> Result<Issue, GithubError> {
        self.enter("get_issue").await?;
        self.with(|s| s.issues.get(&(repo.to_string(), issue)).cloned()).ok_or(GithubError::Status { status: 404, what: "issue".into() })
    }

    async fn set_bounty_label(&self, _i: i64, repo: &str, issue: i32, label: &str) -> Result<(), GithubError> {
        self.enter("set_bounty_label").await?;
        self.with(|s| s.labels.push((repo.into(), issue, label.into())));
        Ok(())
    }

    async fn get_pull(&self, _i: i64, repo: &str, pr: i32) -> Result<Pull, GithubError> {
        self.enter("get_pull").await?;
        self.with(|s| s.pulls.get(&(repo.to_string(), pr)).cloned()).ok_or(GithubError::Status { status: 404, what: "pull".into() })
    }

    async fn get_pull_files(&self, _i: i64, repo: &str, pr: i32) -> Result<Vec<PullFile>, GithubError> {
        self.enter("get_pull_files").await?;
        Ok(self.with(|s| s.files.get(&(repo.to_string(), pr)).cloned().unwrap_or_default()))
    }

    async fn get_ci_conclusion(&self, _i: i64, _repo: &str, _sha: &str) -> Result<CiConclusion, GithubError> {
        self.enter("get_ci_conclusion").await?;
        Ok(self.with(|s| s.ci.unwrap_or(CiConclusion::None)))
    }

    async fn get_readme_excerpt(&self, _i: i64, _repo: &str, _max: usize) -> Result<String, GithubError> {
        self.enter("get_readme_excerpt").await?;
        Ok("# readme".into())
    }

    async fn list_installations(&self) -> Result<Vec<InstallationInfo>, GithubError> {
        self.enter("list_installations").await?;
        Ok(self.with(|s| s.installations.clone()))
    }

    async fn list_repositories(&self, _i: i64) -> Result<Vec<RepoInfo>, GithubError> {
        self.enter("list_repositories").await?;
        Ok(self.with(|s| s.repos.clone()))
    }
}
