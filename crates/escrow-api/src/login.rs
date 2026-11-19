//! The network side of GitHub sign-in, behind a trait so route tests need no GitHub.

use async_trait::async_trait;
use escrow_github::{GithubUser, OAuthClient, OAuthError};

#[async_trait]
pub trait GithubLogin: Send + Sync {
    fn client_id(&self) -> &str;
    async fn exchange_code(&self, code: &str, redirect_uri: &str, verifier: &str) -> Result<String, OAuthError>;
    async fn fetch_user(&self, access_token: &str) -> Result<GithubUser, OAuthError>;
}

pub struct RealLogin {
    pub client_id: String,
    pub client: OAuthClient,
}

#[async_trait]
impl GithubLogin for RealLogin {
    fn client_id(&self) -> &str {
        &self.client_id
    }
    async fn exchange_code(&self, code: &str, redirect_uri: &str, verifier: &str) -> Result<String, OAuthError> {
        self.client.exchange_code(code, redirect_uri, verifier).await
    }
    async fn fetch_user(&self, access_token: &str) -> Result<GithubUser, OAuthError> {
        self.client.fetch_user(access_token).await
    }
}
