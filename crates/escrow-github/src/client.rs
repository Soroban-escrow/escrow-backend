//! The GitHub App client: App JWT -> installation access token (cached, refreshed on expiry or a 401)
//! -> REST. Every path segment that comes from outside is validated before it is put in a URL, so a
//! hostile repo name or login can never redirect a request to another endpoint.

use crate::{
    api::{CiConclusion, GithubApi, InstallationInfo, Issue, Pull, PullFile, RepoInfo},
    error::GithubError,
};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use regex::Regex;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::LazyLock, time::Duration};
use tokio::sync::Mutex;

static REPO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$").unwrap());
static LOGIN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9-]+(\[bot\])?$").unwrap());
static SHA: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?i)[0-9a-f]{7,64}$").unwrap());
static LABEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^bounty: [A-Za-z0-9_ .]{1,40}$").unwrap());

fn repo_ok(r: &str) -> Result<&str, GithubError> {
    let dots = r.split('/').any(|s| s == "." || s == "..");
    if REPO.is_match(r) && !dots { Ok(r) } else { Err(GithubError::Invalid("invalid repository name".into())) }
}

fn positive(n: i64, what: &str) -> Result<i64, GithubError> {
    if n >= 1 { Ok(n) } else { Err(GithubError::Invalid(format!("invalid {what}"))) }
}

pub struct GithubClientConfig {
    pub app_id: String,
    /// PEM, with real newlines or literal `\n`
    pub private_key: String,
    pub api_url: String,
    pub timeout: Duration,
}

impl GithubClientConfig {
    pub fn new(app_id: &str, private_key: &str) -> Self {
        Self {
            app_id: app_id.into(),
            private_key: private_key.into(),
            api_url: "https://api.github.com".into(),
            timeout: Duration::from_secs(20),
        }
    }
}

#[derive(Serialize)]
struct AppClaims<'a> {
    iat: i64,
    exp: i64,
    iss: &'a str,
}

struct CachedToken {
    token: String,
    expires_at: DateTime<Utc>,
}

pub struct GithubClient {
    http: reqwest::Client,
    api: String,
    app_id: String,
    key: EncodingKey,
    tokens: Mutex<HashMap<i64, CachedToken>>,
    now: Box<dyn Fn() -> DateTime<Utc> + Send + Sync>,
}

impl GithubClient {
    pub fn new(cfg: GithubClientConfig) -> Result<Self, GithubError> {
        let pem = cfg.private_key.replace("\\n", "\n");
        let key = EncodingKey::from_rsa_pem(pem.as_bytes())
            .map_err(|_| GithubError::Invalid("GITHUB_APP_PRIVATE_KEY is not a valid RSA PEM key".into()))?;
        let http = reqwest::Client::builder().timeout(cfg.timeout).user_agent("skillescrow-backend").build()?;
        Ok(Self {
            http,
            api: cfg.api_url.trim_end_matches('/').to_string(),
            app_id: cfg.app_id,
            key,
            tokens: Mutex::default(),
            now: Box::new(Utc::now),
        })
    }

    /// Replaces the clock (tests).
    pub fn with_clock(mut self, now: impl Fn() -> DateTime<Utc> + Send + Sync + 'static) -> Self {
        self.now = Box::new(now);
        self
    }

    fn app_jwt(&self) -> Result<String, GithubError> {
        let t = (self.now)().timestamp();
        // iat is back-dated 30 s for clock skew; GitHub allows at most 10 minutes of lifetime
        let claims = AppClaims { iat: t - 30, exp: t + 540, iss: &self.app_id };
        jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &self.key)
            .map_err(|e| GithubError::Invalid(format!("could not sign the App JWT: {e}")))
    }

    async fn installation_token(&self, id: i64, force: bool) -> Result<String, GithubError> {
        let mut cache = self.tokens.lock().await;
        let now = (self.now)();
        if !force
            && let Some(t) = cache.get(&id)
            && t.expires_at - chrono::Duration::seconds(60) > now
        {
            return Ok(t.token.clone());
        }
        #[derive(Deserialize)]
        struct Resp {
            token: String,
            expires_at: DateTime<Utc>,
        }
        let r = self
            .http
            .post(format!("{}/app/installations/{id}/access_tokens", self.api))
            .headers(self.base_headers())
            .bearer_auth(self.app_jwt()?)
            .send()
            .await?;
        if !r.status().is_success() {
            return Err(GithubError::Status { status: r.status().as_u16(), what: "installation token".into() });
        }
        let j: Resp = r.json().await.map_err(|e| GithubError::Decode(e.to_string()))?;
        cache.insert(id, CachedToken { token: j.token.clone(), expires_at: j.expires_at });
        Ok(j.token)
    }

    fn base_headers(&self) -> reqwest::header::HeaderMap {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("accept", "application/vnd.github+json".parse().unwrap());
        h.insert("x-github-api-version", "2022-11-28".parse().unwrap());
        h
    }

    /// One authenticated call. A 401 means the cached token was revoked or expired early: fetch a new one, once.
    async fn call(
        &self,
        installation: i64,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<reqwest::Response, GithubError> {
        for attempt in 0..2 {
            let token = self.installation_token(installation, attempt == 1).await?;
            let mut req = self.http.request(method.clone(), format!("{}{path}", self.api)).headers(self.base_headers()).bearer_auth(token);
            if let Some(b) = body {
                req = req.json(b);
            }
            let resp = req.send().await?;
            if resp.status().as_u16() == 401 && attempt == 0 {
                continue;
            }
            return Ok(resp);
        }
        unreachable!("the loop returns on its second pass")
    }

    async fn json<T: DeserializeOwned>(
        &self,
        installation: i64,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<T, GithubError> {
        let what = format!("{method} {}", path.split('?').next().unwrap_or(path));
        let r = self.call(installation, method, path, body).await?;
        if !r.status().is_success() {
            return Err(GithubError::Status { status: r.status().as_u16(), what });
        }
        r.json().await.map_err(|e| GithubError::Decode(e.to_string()))
    }
}

#[async_trait]
impl GithubApi for GithubClient {
    async fn get_permission(&self, installation: i64, repo: &str, login: &str) -> Result<String, GithubError> {
        if !LOGIN.is_match(login) {
            return Err(GithubError::Invalid("invalid login".into()));
        }
        let r = self
            .call(installation, reqwest::Method::GET, &format!("/repos/{}/collaborators/{login}/permission", repo_ok(repo)?), None)
            .await?;
        if r.status().as_u16() == 404 {
            return Ok("none".into());
        }
        if !r.status().is_success() {
            return Err(GithubError::Status { status: r.status().as_u16(), what: "permission lookup".into() });
        }
        #[derive(Deserialize)]
        struct P {
            permission: String,
        }
        Ok(r.json::<P>().await.map_err(|e| GithubError::Decode(e.to_string()))?.permission)
    }

    async fn post_comment(&self, installation: i64, repo: &str, issue: i32, body: &str) -> Result<i64, GithubError> {
        #[derive(Deserialize)]
        struct C {
            id: i64,
        }
        let path = format!("/repos/{}/issues/{}/comments", repo_ok(repo)?, positive(issue.into(), "issue")?);
        Ok(self.json::<C>(installation, reqwest::Method::POST, &path, Some(&json!({ "body": body }))).await?.id)
    }

    async fn update_comment(&self, installation: i64, repo: &str, comment_id: i64, body: &str) -> Result<(), GithubError> {
        let path = format!("/repos/{}/issues/comments/{}", repo_ok(repo)?, positive(comment_id, "comment id")?);
        self.json::<Value>(installation, reqwest::Method::PATCH, &path, Some(&json!({ "body": body }))).await.map(|_| ())
    }

    async fn get_issue(&self, installation: i64, repo: &str, issue: i32) -> Result<Issue, GithubError> {
        let path = format!("/repos/{}/issues/{}", repo_ok(repo)?, positive(issue.into(), "issue")?);
        self.json(installation, reqwest::Method::GET, &path, None).await
    }

    async fn set_bounty_label(&self, installation: i64, repo: &str, issue: i32, label: &str) -> Result<(), GithubError> {
        let repo = repo_ok(repo)?;
        let issue = positive(issue.into(), "issue")?;
        if !LABEL.is_match(label) {
            return Err(GithubError::Invalid("invalid label".into()));
        }
        #[derive(Deserialize)]
        struct L {
            name: String,
        }
        let existing: Vec<L> =
            self.json(installation, reqwest::Method::GET, &format!("/repos/{repo}/issues/{issue}/labels?per_page=100"), None).await?;
        for l in existing.iter().filter(|l| l.name.starts_with("bounty:") && l.name != label) {
            let enc = l
                .name
                .bytes()
                .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
                .collect::<String>();
            self.call(installation, reqwest::Method::DELETE, &format!("/repos/{repo}/issues/{issue}/labels/{enc}"), None).await?;
        }
        let made = self
            .call(installation, reqwest::Method::POST, &format!("/repos/{repo}/labels"), Some(&json!({ "name": label, "color": "c9f25d" })))
            .await?;
        // 422 = the label already exists, which is fine
        if !made.status().is_success() && made.status().as_u16() != 422 {
            return Err(GithubError::Status { status: made.status().as_u16(), what: "create label".into() });
        }
        self.json::<Value>(
            installation,
            reqwest::Method::POST,
            &format!("/repos/{repo}/issues/{issue}/labels"),
            Some(&json!({ "labels": [label] })),
        )
        .await
        .map(|_| ())
    }

    async fn get_pull(&self, installation: i64, repo: &str, pr: i32) -> Result<Pull, GithubError> {
        let path = format!("/repos/{}/pulls/{}", repo_ok(repo)?, positive(pr.into(), "pull request")?);
        self.json(installation, reqwest::Method::GET, &path, None).await
    }

    /// Changed files with patches, paginated and capped at 300 files.
    async fn get_pull_files(&self, installation: i64, repo: &str, pr: i32) -> Result<Vec<PullFile>, GithubError> {
        let repo = repo_ok(repo)?;
        let pr = positive(pr.into(), "pull request")?;
        let mut out = Vec::new();
        for page in 1..=3 {
            let files: Vec<PullFile> = self
                .json(installation, reqwest::Method::GET, &format!("/repos/{repo}/pulls/{pr}/files?per_page=100&page={page}"), None)
                .await?;
            let n = files.len();
            out.extend(files);
            if n < 100 {
                break;
            }
        }
        Ok(out)
    }

    async fn get_ci_conclusion(&self, installation: i64, repo: &str, sha: &str) -> Result<CiConclusion, GithubError> {
        if !SHA.is_match(sha) {
            return Err(GithubError::Invalid("invalid sha".into()));
        }
        #[derive(Deserialize)]
        struct Runs {
            check_runs: Vec<Run>,
        }
        #[derive(Deserialize)]
        struct Run {
            status: String,
            conclusion: Option<String>,
        }
        let j: Runs = self
            .json(installation, reqwest::Method::GET, &format!("/repos/{}/commits/{sha}/check-runs?per_page=100", repo_ok(repo)?), None)
            .await?;
        Ok(if j.check_runs.is_empty() {
            CiConclusion::None
        } else if j.check_runs.iter().any(|c| c.status != "completed") {
            CiConclusion::Pending
        } else if j.check_runs.iter().all(|c| matches!(c.conclusion.as_deref(), Some("success" | "neutral" | "skipped"))) {
            CiConclusion::Success
        } else {
            CiConclusion::Failure
        })
    }

    async fn get_readme_excerpt(&self, installation: i64, repo: &str, max_chars: usize) -> Result<String, GithubError> {
        let r = self.call(installation, reqwest::Method::GET, &format!("/repos/{}/readme", repo_ok(repo)?), None).await?;
        if !r.status().is_success() {
            return Ok(String::new());
        }
        #[derive(Deserialize)]
        struct R {
            content: Option<String>,
            encoding: Option<String>,
        }
        let j: R = r.json().await.map_err(|e| GithubError::Decode(e.to_string()))?;
        if j.encoding.as_deref() != Some("base64") {
            return Ok(String::new());
        }
        let raw = j.content.unwrap_or_default().replace('\n', "");
        let bytes = STANDARD.decode(raw).unwrap_or_default();
        Ok(String::from_utf8_lossy(&bytes).chars().take(max_chars).collect())
    }

    async fn list_installations(&self) -> Result<Vec<InstallationInfo>, GithubError> {
        let r = self
            .http
            .get(format!("{}/app/installations?per_page=100", self.api))
            .headers(self.base_headers())
            .bearer_auth(self.app_jwt()?)
            .send()
            .await?;
        if !r.status().is_success() {
            return Err(GithubError::Status { status: r.status().as_u16(), what: "list installations".into() });
        }
        r.json().await.map_err(|e| GithubError::Decode(e.to_string()))
    }

    async fn list_repositories(&self, installation: i64) -> Result<Vec<RepoInfo>, GithubError> {
        #[derive(Deserialize)]
        struct R {
            repositories: Vec<RepoInfo>,
        }
        Ok(self.json::<R>(installation, reqwest::Method::GET, "/installation/repositories?per_page=100", None).await?.repositories)
    }
}
