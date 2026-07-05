//! The network half of "Sign in with GitHub": exchange the code (with the PKCE verifier) and read the profile.
//! The pure half (state cookie, PKCE challenge, redirect allow-list) is in `escrow_core::oauth`.

use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubUser {
    pub id: i64,
    pub login: String,
    pub avatar_url: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum OAuthError {
    /// GitHub refused the code (expired, reused, wrong verifier, ...)
    Rejected(String),
    /// GitHub could not be reached or answered nonsense
    Unavailable,
}

pub struct OAuthClient {
    http: reqwest::Client,
    client_id: String,
    client_secret: String,
    /// `https://github.com` in production
    web_base: String,
    /// `https://api.github.com` in production
    api_base: String,
}

impl OAuthClient {
    pub fn new(client_id: &str, client_secret: &str) -> Self {
        Self::with_bases(client_id, client_secret, "https://github.com", "https://api.github.com")
    }

    pub fn with_bases(client_id: &str, client_secret: &str, web_base: &str, api_base: &str) -> Self {
        let http =
            reqwest::Client::builder().timeout(Duration::from_secs(15)).user_agent("skillescrow-backend").build().expect("client builds");
        Self {
            http,
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            web_base: web_base.trim_end_matches('/').into(),
            api_base: api_base.trim_end_matches('/').into(),
        }
    }

    pub async fn exchange_code(&self, code: &str, redirect_uri: &str, verifier: &str) -> Result<String, OAuthError> {
        #[derive(Deserialize)]
        struct T {
            access_token: Option<String>,
            error: Option<String>,
        }
        let r = self
            .http
            .post(format!("{}/login/oauth/access_token", self.web_base))
            .header("accept", "application/json")
            .json(&json!({ "client_id": self.client_id, "client_secret": self.client_secret, "code": code, "redirect_uri": redirect_uri, "code_verifier": verifier }))
            .send()
            .await
            .map_err(|_| OAuthError::Unavailable)?;
        let ok = r.status().is_success();
        let t: T = r.json().await.unwrap_or(T { access_token: None, error: None });
        match (ok, t.access_token) {
            (true, Some(tok)) => Ok(tok),
            _ => Err(OAuthError::Rejected(t.error.unwrap_or_default())),
        }
    }

    pub async fn fetch_user(&self, access_token: &str) -> Result<GithubUser, OAuthError> {
        #[derive(Deserialize)]
        struct U {
            id: i64,
            login: String,
            avatar_url: Option<String>,
        }
        let r = self
            .http
            .get(format!("{}/user", self.api_base))
            .header("accept", "application/vnd.github+json")
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|_| OAuthError::Unavailable)?;
        if !r.status().is_success() {
            return Err(OAuthError::Unavailable);
        }
        let u: U = r.json().await.map_err(|_| OAuthError::Unavailable)?;
        if u.id < 1 || u.login.is_empty() {
            return Err(OAuthError::Unavailable);
        }
        Ok(GithubUser { id: u.id, login: u.login, avatar_url: u.avatar_url })
    }
}
