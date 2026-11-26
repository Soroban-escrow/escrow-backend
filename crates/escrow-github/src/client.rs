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
use std::{
    collections::HashMap,
    sync::LazyLock,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

static REPO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$").unwrap());
static LOGIN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9-]+(\[bot\])?$").unwrap());
static SHA: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?i)[0-9a-f]{7,64}$").unwrap());
/// Stop calling an installation when fewer than this many primary-limit calls remain before its reset.
const SAFETY_MARGIN: i64 = 10;
/// GitHub asks for about one second between mutating requests.
const WRITE_SPACING: Duration = Duration::from_secs(1);
/// Pause for a 429 or a limited 403 that names no wait time.
const DEFAULT_PAUSE_SECS: i64 = 60;
/// Bound on cached ETag responses, so the cache cannot grow without limit.
const ETAG_CACHE_MAX: usize = 1024;
/// Bound on paginated list calls, so a looping Link header can never spin forever.
const MAX_LIST_PAGES: usize = 100;
/// How long a permission answer (write / none / ...) is reused before asking GitHub again.
const PERMISSION_TTL_SECS: i64 = 60;
/// (installation, repo, login) -> (permission, expires at): only "ok" answers are cached, never errors
type PermissionCache = HashMap<(i64, String, String), (String, DateTime<Utc>)>;

static LABEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^bounty: [A-Za-z0-9_ .]{1,40}$").unwrap());
static FILE_PATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_./-]{1,300}$").unwrap());
/// Files bigger than this are not read for review context.
const MAX_CONTEXT_FILE_BYTES: u64 = 200_000;

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

/// What we know about one installation's rate limits. Each installation has its own, so one throttled
/// installation never stops another.
#[derive(Default)]
struct Throttle {
    /// Do not call this installation again before this time.
    paused_until: Option<DateTime<Utc>>,
    remaining: Option<i64>,
    reset_at: Option<DateTime<Utc>>,
    last_write: Option<Instant>,
}

/// Given a response's status and limit headers, the time before which the installation must not be called again.
/// `None` means the response is not a rate limit (a 403 for permissions, say) and is handled as a normal status.
pub(crate) fn pause_until(
    status: u16,
    retry_after: Option<i64>,
    remaining: Option<i64>,
    reset: Option<i64>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    if status != 403 && status != 429 {
        return None;
    }
    // secondary limits name the wait exactly
    if let Some(s) = retry_after {
        return Some(now + chrono::Duration::seconds(s.max(1)));
    }
    // primary limit: the window resets at the reset time
    if remaining == Some(0) {
        return Some(reset.and_then(|r| DateTime::from_timestamp(r, 0)).unwrap_or(now + chrono::Duration::seconds(DEFAULT_PAUSE_SECS)));
    }
    if status == 429 {
        return Some(now + chrono::Duration::seconds(DEFAULT_PAUSE_SECS));
    }
    None
}

/// Mutating requests: the ones GitHub asks to be spaced out.
fn is_write(m: &reqwest::Method) -> bool {
    *m == reqwest::Method::POST || *m == reqwest::Method::PATCH || *m == reqwest::Method::PUT || *m == reqwest::Method::DELETE
}

/// How long a write must wait so that writes are spaced by `WRITE_SPACING`.
pub(crate) fn write_wait(last: Option<Instant>, now: Instant) -> Duration {
    match last {
        Some(t) => WRITE_SPACING.saturating_sub(now.duration_since(t)),
        None => Duration::ZERO,
    }
}

/// Extracts the `rel="next"` path from a GitHub `Link` header, rejecting cross-origin URLs.
pub(crate) fn next_link(headers: &reqwest::header::HeaderMap, api_base: &str) -> Option<String> {
    let raw = headers.get(reqwest::header::LINK)?.to_str().ok()?;
    for part in raw.split(',') {
        let mut segments = part.split(';').map(str::trim);
        let url = segments.next()?.strip_prefix('<')?.strip_suffix('>')?;
        if segments.any(|s| s == r#"rel="next""# || s == "rel=next")
            && let Some(rest) = url.strip_prefix(api_base)
            && rest.starts_with('/')
        {
            return Some(rest.to_string());
        }
    }
    None
}

pub struct GithubClient {
    http: reqwest::Client,
    api: String,
    app_id: String,
    key: EncodingKey,
    tokens: Mutex<HashMap<i64, CachedToken>>,
    throttles: Mutex<HashMap<i64, Throttle>>,
    /// installation + path -> (ETag, body) for conditional GETs
    etags: Mutex<HashMap<String, (String, Vec<u8>)>>,
    /// (installation, repo, login) -> (permission, expires at): only "ok" answers are cached, never errors
    permissions: Mutex<PermissionCache>,
    now: Box<dyn Fn() -> DateTime<Utc> + Send + Sync>,
}

impl GithubClient {
    pub fn new(cfg: GithubClientConfig) -> Result<Self, GithubError> {
        let pem = cfg.private_key.replace("\\n", "\n");
        let key = EncodingKey::from_rsa_pem(pem.as_bytes())
            .map_err(|_| GithubError::Invalid("GITHUB_APP_PRIVATE_KEY is not a valid RSA PEM key".into()))?;
        let http = reqwest::Client::builder().timeout(cfg.timeout).user_agent(escrow_core::USER_AGENT).build()?;
        Ok(Self {
            http,
            api: cfg.api_url.trim_end_matches('/').to_string(),
            app_id: cfg.app_id,
            key,
            tokens: Mutex::default(),
            throttles: Mutex::default(),
            etags: Mutex::default(),
            permissions: Mutex::default(),
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

    /// Waits for this installation's limits to allow a call. Fails fast with `RateLimited` when it is paused or
    /// nearly out of primary quota; spaces mutating requests by `WRITE_SPACING`.
    async fn gate(&self, installation: i64, method: &reqwest::Method, what: &str) -> Result<(), GithubError> {
        let now = (self.now)();
        let wait = {
            let mut all = self.throttles.lock().await;
            let t = all.entry(installation).or_default();
            if let Some(until) = t.paused_until
                && until > now
            {
                return Err(GithubError::RateLimited { retry_at: until, what: what.into() });
            }
            if let (Some(r), Some(reset)) = (t.remaining, t.reset_at)
                && r < SAFETY_MARGIN
                && reset > now
            {
                return Err(GithubError::RateLimited { retry_at: reset, what: what.into() });
            }
            let writes = is_write(method);
            if writes { write_wait(t.last_write, Instant::now()) } else { Duration::ZERO }
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        if is_write(method) {
            self.throttles.lock().await.entry(installation).or_default().last_write = Some(Instant::now());
        }
        Ok(())
    }

    /// Reads the limit headers of a response and remembers them for this installation.
    async fn remember(&self, installation: i64, resp: &reqwest::Response) -> Option<DateTime<Utc>> {
        let header = |name: &str| resp.headers().get(name).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<i64>().ok());
        let now = (self.now)();
        let (retry_after, remaining, reset) = (header("retry-after"), header("x-ratelimit-remaining"), header("x-ratelimit-reset"));
        let mut all = self.throttles.lock().await;
        let t = all.entry(installation).or_default();
        if let Some(r) = remaining {
            t.remaining = Some(r);
            t.reset_at = reset.and_then(|s| DateTime::from_timestamp(s, 0));
        }
        let until = pause_until(resp.status().as_u16(), retry_after, remaining, reset, now);
        if let Some(u) = until {
            t.paused_until = Some(u.max(t.paused_until.unwrap_or(u)));
        }
        until
    }

    /// One authenticated call. A 401 means the cached token was revoked or expired early: fetch a new one, once.
    async fn call(
        &self,
        installation: i64,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<reqwest::Response, GithubError> {
        self.call_with(installation, method, path, body, None).await
    }

    async fn call_with(
        &self,
        installation: i64,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
        if_none_match: Option<&str>,
    ) -> Result<reqwest::Response, GithubError> {
        let what = format!("{method} {}", path.split('?').next().unwrap_or(path));
        for attempt in 0..2 {
            self.gate(installation, &method, &what).await?;
            let token = self.installation_token(installation, attempt == 1).await?;
            let mut req = self.http.request(method.clone(), format!("{}{path}", self.api)).headers(self.base_headers()).bearer_auth(token);
            if let Some(tag) = if_none_match {
                req = req.header(reqwest::header::IF_NONE_MATCH, tag);
            }
            if let Some(b) = body {
                req = req.json(b);
            }
            let resp = req.send().await?;
            if resp.status().as_u16() == 401 && attempt == 0 {
                continue;
            }
            if let Some(retry_at) = self.remember(installation, &resp).await {
                return Err(GithubError::RateLimited { retry_at, what });
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
        let cacheable = method == reqwest::Method::GET;
        let key = format!("{installation}:{path}");
        let cached = if cacheable { self.etags.lock().await.get(&key).cloned() } else { None };
        let r = self.call_with(installation, method, path, body, cached.as_ref().map(|(tag, _)| tag.as_str())).await?;
        // 304: the cached body is still current, and GitHub does not count it against the quota
        if r.status().as_u16() == 304
            && let Some((_, bytes)) = cached
        {
            return serde_json::from_slice(&bytes).map_err(|e| GithubError::Decode(e.to_string()));
        }
        if !r.status().is_success() {
            return Err(GithubError::Status { status: r.status().as_u16(), what });
        }
        let etag = r.headers().get(reqwest::header::ETAG).and_then(|v| v.to_str().ok()).map(str::to_owned);
        if cacheable && let Some(tag) = etag {
            let bytes = r.bytes().await?.to_vec();
            let value = serde_json::from_slice(&bytes).map_err(|e| GithubError::Decode(e.to_string()))?;
            let mut cache = self.etags.lock().await;
            if cache.len() >= ETAG_CACHE_MAX {
                cache.clear();
            }
            cache.insert(key, (tag, bytes));
            return Ok(value);
        }
        r.json().await.map_err(|e| GithubError::Decode(e.to_string()))
    }
    async fn delete_bounty_labels(&self, installation: i64, repo: &str, issue: i64, keep: Option<&str>) -> Result<(), GithubError> {
        #[derive(Deserialize)]
        struct L {
            name: String,
        }
        let existing: Vec<L> =
            self.json(installation, reqwest::Method::GET, &format!("/repos/{repo}/issues/{issue}/labels?per_page=100"), None).await?;
        for l in existing.iter().filter(|l| escrow_core::card::is_bounty_label(&l.name) && keep != Some(l.name.as_str())) {
            let enc = l
                .name
                .bytes()
                .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
                .collect::<String>();
            self.call(installation, reqwest::Method::DELETE, &format!("/repos/{repo}/issues/{issue}/labels/{enc}"), None).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl GithubApi for GithubClient {
    async fn get_permission(&self, installation: i64, repo: &str, login: &str) -> Result<String, GithubError> {
        if !LOGIN.is_match(login) {
            return Err(GithubError::Invalid("invalid login".into()));
        }
        let key = (installation, repo.to_owned(), login.to_owned());
        let now = (self.now)();
        if let Some((permission, expires_at)) = self.permissions.lock().await.get(&key)
            && *expires_at > now
        {
            return Ok(permission.clone());
        }
        let r = self
            .call(installation, reqwest::Method::GET, &format!("/repos/{}/collaborators/{login}/permission", repo_ok(repo)?), None)
            .await?;
        let permission = if r.status().as_u16() == 404 {
            "none".into()
        } else if !r.status().is_success() {
            return Err(GithubError::Status { status: r.status().as_u16(), what: "permission lookup".into() });
        } else {
            #[derive(Deserialize)]
            struct P {
                permission: String,
            }
            r.json::<P>().await.map_err(|e| GithubError::Decode(e.to_string()))?.permission
        };
        // only an answer GitHub actually gave is worth caching; a failure is never cached
        self.permissions.lock().await.insert(key, (permission.clone(), now + chrono::Duration::seconds(PERMISSION_TTL_SECS)));
        Ok(permission)
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
        self.delete_bounty_labels(installation, repo, issue, Some(label)).await?;
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

    async fn remove_bounty_label(&self, installation: i64, repo: &str, issue: i32) -> Result<(), GithubError> {
        let repo = repo_ok(repo)?;
        let issue = positive(issue.into(), "issue")?;
        self.delete_bounty_labels(installation, repo, issue, None).await
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

    async fn get_file_at(&self, installation: i64, repo: &str, git_ref: Option<&str>, path: &str) -> Result<Option<String>, GithubError> {
        if !FILE_PATH.is_match(path) || path.split('/').any(|s| s.is_empty() || s == "." || s == "..") {
            return Err(GithubError::Invalid("invalid file path".into()));
        }
        let mut url = format!("/repos/{}/contents/{path}", repo_ok(repo)?);
        if let Some(r) = git_ref {
            if !SHA.is_match(r) {
                return Err(GithubError::Invalid("invalid ref".into()));
            }
            url.push_str(&format!("?ref={r}"));
        }
        let r = self.call(installation, reqwest::Method::GET, &url, None).await?;
        if r.status().as_u16() == 404 {
            return Ok(None);
        }
        if !r.status().is_success() {
            return Err(GithubError::Status { status: r.status().as_u16(), what: "file read".into() });
        }
        #[derive(Deserialize)]
        struct F {
            #[serde(rename = "type")]
            kind: Option<String>,
            encoding: Option<String>,
            size: Option<u64>,
            content: Option<String>,
        }
        let f: F = r.json().await.map_err(|e| GithubError::Decode(e.to_string()))?;
        if f.kind.as_deref() != Some("file") || f.encoding.as_deref() != Some("base64") || f.size.unwrap_or(0) > MAX_CONTEXT_FILE_BYTES {
            return Ok(None);
        }
        let bytes = STANDARD.decode(f.content.unwrap_or_default().replace('\n', "")).unwrap_or_default();
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }

    async fn list_installations(&self) -> Result<Vec<InstallationInfo>, GithubError> {
        let mut out = Vec::new();
        let mut next = Some("/app/installations?per_page=100".to_string());
        for _ in 0..MAX_LIST_PAGES {
            let Some(path) = next else { break };
            let r = self.http.get(format!("{}{path}", self.api)).headers(self.base_headers()).bearer_auth(self.app_jwt()?).send().await?;
            if !r.status().is_success() {
                return Err(GithubError::Status { status: r.status().as_u16(), what: "list installations".into() });
            }
            next = next_link(r.headers(), &self.api);
            let page: Vec<InstallationInfo> = r.json().await.map_err(|e| GithubError::Decode(e.to_string()))?;
            out.extend(page);
        }
        Ok(out)
    }

    async fn list_repositories(&self, installation: i64) -> Result<Vec<RepoInfo>, GithubError> {
        #[derive(Deserialize)]
        struct R {
            repositories: Vec<RepoInfo>,
        }
        let mut out = Vec::new();
        let mut next = Some("/installation/repositories?per_page=100".to_string());
        for _ in 0..MAX_LIST_PAGES {
            let Some(path) = next else { break };
            let r = self.call(installation, reqwest::Method::GET, &path, None).await?;
            if !r.status().is_success() {
                return Err(GithubError::Status { status: r.status().as_u16(), what: "list repositories".into() });
            }
            next = next_link(r.headers(), &self.api);
            let page: R = r.json().await.map_err(|e| GithubError::Decode(e.to_string()))?;
            out.extend(page.repositories);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(s: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(s, 0).unwrap()
    }

    #[test]
    fn secondary_limit_honours_retry_after_exactly() {
        assert_eq!(pause_until(403, Some(120), None, None, at(1000)), Some(at(1120)));
        assert_eq!(pause_until(429, Some(0), None, None, at(1000)), Some(at(1001)), "never less than a second");
    }

    #[test]
    fn primary_exhaustion_waits_for_the_reset() {
        assert_eq!(pause_until(403, None, Some(0), Some(1500), at(1000)), Some(at(1500)));
    }

    #[test]
    fn too_many_requests_without_headers_pauses_a_minute() {
        assert_eq!(pause_until(429, None, None, None, at(1000)), Some(at(1060)));
    }

    #[test]
    fn a_plain_permission_403_is_not_a_rate_limit() {
        assert_eq!(pause_until(403, None, Some(4000), Some(1500), at(1000)), None);
        assert_eq!(pause_until(404, Some(60), None, None, at(1000)), None);
    }

    #[test]
    fn writes_are_spaced_by_a_second() {
        let t0 = Instant::now();
        assert_eq!(write_wait(None, t0), Duration::ZERO);
        assert_eq!(write_wait(Some(t0), t0), WRITE_SPACING);
        assert_eq!(write_wait(Some(t0), t0 + WRITE_SPACING), Duration::ZERO);
    }

    #[test]
    fn next_link_parses_same_origin_next_and_rejects_other_hosts() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            reqwest::header::LINK,
            r#"<https://api.github.com/installation/repositories?per_page=100&page=2>; rel="next", <https://api.github.com/installation/repositories?per_page=100&page=4>; rel="last""#
                .parse()
                .unwrap(),
        );
        assert_eq!(next_link(&h, "https://api.github.com"), Some("/installation/repositories?per_page=100&page=2".into()));

        h.insert(
            reqwest::header::LINK,
            r#"<https://api.github.com/installation/repositories?per_page=100&page=1>; rel="prev""#.parse().unwrap(),
        );
        assert_eq!(next_link(&h, "https://api.github.com"), None);

        h.insert(reqwest::header::LINK, r#"<https://evil.example/steal>; rel="next""#.parse().unwrap());
        assert_eq!(next_link(&h, "https://api.github.com"), None);
    }
}
