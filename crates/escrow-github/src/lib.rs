#![forbid(unsafe_code)]
//! GitHub integration: the GitHub App client (JWT -> installation token -> REST), the sign-in OAuth
//! calls, and typed webhook payloads. Everything the rest of the backend needs from GitHub goes
//! through the [`GithubApi`] trait, so the engine and the AI reviewer are tested against fakes.

pub mod api;
pub mod client;
pub mod error;
pub mod oauth;
pub mod payloads;

pub use api::{CiConclusion, GithubApi, InstallationInfo, Issue, Pull, PullFile, PullUser, RepoInfo};
pub use client::{GithubClient, GithubClientConfig};
pub use error::GithubError;
pub use oauth::{GithubUser, OAuthClient, OAuthError};
