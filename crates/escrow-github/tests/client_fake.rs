//! The GitHub client against a fake GitHub. Records every request so the tests can assert on tokens,
//! caching, retries and what is (not) allowed into a URL.

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::IntoResponse,
    routing::any,
};
use chrono::{DateTime, Duration, Utc};
use escrow_github::{CiConclusion, GithubApi, GithubClient, GithubClientConfig, GithubError, OAuthClient, OAuthError};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, OnceLock};

struct Keys {
    private_pem: String,
    /// DER-encoded RSAPublicKey (PKCS#1)
    public_der: Vec<u8>,
}

fn pem(label: &str, der: &[u8]) -> String {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let body = STANDARD.encode(der).as_bytes().chunks(64).map(|c| std::str::from_utf8(c).unwrap()).collect::<Vec<_>>().join("\n");
    format!("-----BEGIN {label}-----\n{body}\n-----END {label}-----\n")
}

/// A throwaway RSA key generated per test run: no private key is ever committed to the repository.
fn keys() -> &'static Keys {
    static K: OnceLock<Keys> = OnceLock::new();
    K.get_or_init(|| {
        use aws_lc_rs::{
            encoding::AsDer,
            rsa::{KeyPair, KeySize},
            signature::KeyPair as _,
        };
        let kp = KeyPair::generate(KeySize::Rsa2048).unwrap();
        let pkcs8 = kp.as_der().unwrap();
        Keys { private_pem: pem("PRIVATE KEY", pkcs8.as_ref()), public_der: kp.public_key().as_ref().to_vec() }
    })
}

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    auth: String,
    body: Value,
}

type Handler = Arc<dyn Fn(&Seen) -> (u16, Value) + Send + Sync>;
struct Fake {
    url: String,
    log: Arc<Mutex<Vec<Seen>>>,
}

impl Fake {
    async fn start(h: impl Fn(&Seen) -> (u16, Value) + Send + Sync + 'static) -> Fake {
        let log: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let state = (log.clone(), Arc::new(h) as Handler);
        async fn handle(
            State((log, h)): State<(Arc<Mutex<Vec<Seen>>>, Handler)>,
            method: Method,
            uri: Uri,
            headers: HeaderMap,
            body: axum::body::Bytes,
        ) -> impl IntoResponse {
            let seen = Seen {
                method: method.to_string(),
                path: uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_default(),
                auth: headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string(),
                body: serde_json::from_slice(&body).unwrap_or(Value::Null),
            };
            let (code, out) = h(&seen);
            log.lock().unwrap().push(seen);
            (StatusCode::from_u16(code).unwrap(), Json(out))
        }
        let app = Router::new().fallback(any(handle)).with_state(state);
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        Fake { url, log }
    }
    fn seen(&self) -> Vec<Seen> {
        self.log.lock().unwrap().clone()
    }
    fn client(&self) -> GithubClient {
        let mut cfg = GithubClientConfig::new("12345", &keys().private_pem);
        cfg.api_url = self.url.clone();
        GithubClient::new(cfg).unwrap()
    }
}

fn token_reply(token: &str, expires: DateTime<Utc>) -> (u16, Value) {
    (201, json!({ "token": token, "expires_at": expires.to_rfc3339() }))
}

/// A server that mints numbered tokens and serves `ok_body` for everything else.
fn simple(ok_body: Value) -> impl Fn(&Seen) -> (u16, Value) + Send + Sync + 'static {
    let n = Arc::new(Mutex::new(0));
    move |s| {
        if s.path.ends_with("/access_tokens") {
            let mut n = n.lock().unwrap();
            *n += 1;
            return token_reply(&format!("tok-{n}"), Utc::now() + Duration::hours(1));
        }
        (200, ok_body.clone())
    }
}

#[tokio::test]
async fn the_app_jwt_is_a_valid_rs256_token_for_the_right_app() {
    let fake = Fake::start(simple(json!([]))).await;
    fake.client().list_installations().await.unwrap();
    let auth = &fake.seen()[0].auth;
    let jwt = auth.strip_prefix("Bearer ").unwrap();
    let mut v = Validation::new(Algorithm::RS256);
    v.set_issuer(&["12345"]);
    let data = jsonwebtoken::decode::<Value>(jwt, &DecodingKey::from_rsa_der(&keys().public_der), &v).unwrap();
    let (iat, exp) = (data.claims["iat"].as_i64().unwrap(), data.claims["exp"].as_i64().unwrap());
    assert!(exp - iat <= 600, "GitHub rejects App JWTs valid for more than 10 minutes");
    assert!(iat <= Utc::now().timestamp(), "iat is back-dated for clock skew");
}

#[tokio::test]
async fn an_invalid_private_key_is_rejected_at_construction() {
    let mut cfg = GithubClientConfig::new("1", "not a key");
    cfg.api_url = "http://localhost".into();
    assert!(matches!(GithubClient::new(cfg), Err(GithubError::Invalid(_))));
}

#[tokio::test]
async fn a_private_key_with_literal_backslash_n_works() {
    let fake = Fake::start(simple(json!([]))).await;
    let mut cfg = GithubClientConfig::new("12345", &keys().private_pem.replace('\n', "\\n"));
    cfg.api_url = fake.url.clone();
    GithubClient::new(cfg).unwrap().list_installations().await.unwrap();
}

#[tokio::test]
async fn installation_tokens_are_cached_per_installation() {
    let fake = Fake::start(simple(json!({ "id": 1, "title": "t", "number": 3, "state": "open", "html_url": "u", "body": null }))).await;
    let c = fake.client();
    for _ in 0..3 {
        c.get_issue(7, "acme/widgets", 3).await.unwrap();
    }
    c.get_issue(8, "acme/widgets", 3).await.unwrap();
    let mints = fake.seen().iter().filter(|s| s.path.ends_with("/access_tokens")).count();
    assert_eq!(mints, 2, "one token per installation, reused");
    assert!(fake.seen().iter().any(|s| s.path == "/app/installations/7/access_tokens"));
}

#[tokio::test]
async fn a_token_close_to_expiry_is_replaced() {
    let n = Arc::new(Mutex::new(0));
    let n2 = n.clone();
    let fake = Fake::start(move |s| {
        if s.path.ends_with("/access_tokens") {
            let mut n = n2.lock().unwrap();
            *n += 1;
            // the first token is already within the 60 s refresh margin
            return token_reply(&format!("tok-{n}"), Utc::now() + Duration::seconds(if *n == 1 { 30 } else { 3600 }));
        }
        (200, json!({ "number": 3, "title": "t", "state": "open", "html_url": "u", "body": null }))
    })
    .await;
    let c = fake.client();
    c.get_issue(7, "acme/widgets", 3).await.unwrap();
    c.get_issue(7, "acme/widgets", 3).await.unwrap();
    c.get_issue(7, "acme/widgets", 3).await.unwrap();
    assert_eq!(*n.lock().unwrap(), 2);
}

#[tokio::test]
async fn a_401_refreshes_the_token_once_and_retries() {
    let revoked = Arc::new(Mutex::new(true));
    let r2 = revoked.clone();
    let fake = Fake::start(move |s| {
        if s.path.ends_with("/access_tokens") {
            return token_reply(if *r2.lock().unwrap() { "stale" } else { "fresh" }, Utc::now() + Duration::hours(1));
        }
        if s.auth == "Bearer stale" {
            *r2.lock().unwrap() = false; // the next mint returns a good token
            return (401, json!({ "message": "Bad credentials" }));
        }
        (200, json!({ "number": 3, "title": "t", "state": "open", "html_url": "u", "body": null }))
    })
    .await;
    let issue = fake.client().get_issue(7, "acme/widgets", 3).await.unwrap();
    assert_eq!(issue.number, 3);
    assert_eq!(fake.seen().iter().filter(|s| s.path.ends_with("/access_tokens")).count(), 2);
}

#[tokio::test]
async fn a_persistent_401_is_an_error_not_a_loop() {
    let fake =
        Fake::start(
            |s| if s.path.ends_with("/access_tokens") { token_reply("t", Utc::now() + Duration::hours(1)) } else { (401, json!({})) },
        )
        .await;
    let e = fake.client().get_issue(7, "acme/widgets", 3).await.unwrap_err();
    assert_eq!(e.status(), Some(401));
    assert!(fake.seen().len() <= 4, "bounded retries");
}

#[tokio::test]
async fn permission_lookup_maps_404_to_none_and_rejects_hostile_input() {
    let fake = Fake::start(|s| {
        if s.path.ends_with("/access_tokens") {
            return token_reply("t", Utc::now() + Duration::hours(1));
        }
        if s.path.contains("/collaborators/stranger/") { (404, json!({})) } else { (200, json!({ "permission": "write" })) }
    })
    .await;
    let c = fake.client();
    assert_eq!(c.get_permission(1, "acme/widgets", "alice").await.unwrap(), "write");
    assert_eq!(c.get_permission(1, "acme/widgets", "stranger").await.unwrap(), "none");
    assert_eq!(c.get_permission(1, "acme/widgets", "dependabot[bot]").await.unwrap(), "write");
    let before = fake.seen().len();
    for login in ["../admin", "a/b", "a b", "", "a?x=1", "a%2Fb"] {
        assert!(matches!(c.get_permission(1, "acme/widgets", login).await, Err(GithubError::Invalid(_))), "{login:?}");
    }
    for repo in ["acme", "acme/widgets/extra", "../x", "a/..", "./b", "a/b?x", "a b/c", "a/b#c", ""] {
        assert!(matches!(c.get_issue(1, repo, 1).await, Err(GithubError::Invalid(_))), "{repo:?}");
    }
    assert!(matches!(c.get_issue(1, "a/b", 0).await, Err(GithubError::Invalid(_))));
    assert_eq!(fake.seen().len(), before, "nothing hostile ever reached the network");
}

#[tokio::test]
async fn comments_are_posted_and_edited() {
    let fake = Fake::start(|s| {
        if s.path.ends_with("/access_tokens") {
            return token_reply("t", Utc::now() + Duration::hours(1));
        }
        (200, json!({ "id": 555 }))
    })
    .await;
    let c = fake.client();
    assert_eq!(c.post_comment(1, "acme/widgets", 3, "hello **world**").await.unwrap(), 555);
    c.update_comment(1, "acme/widgets", 555, "edited").await.unwrap();
    let seen = fake.seen();
    let post = seen.iter().find(|s| s.method == "POST" && s.path.ends_with("/comments")).unwrap();
    assert_eq!(post.path, "/repos/acme/widgets/issues/3/comments");
    assert_eq!(post.body, json!({ "body": "hello **world**" }));
    let patch = seen.iter().find(|s| s.method == "PATCH").unwrap();
    assert_eq!(patch.path, "/repos/acme/widgets/issues/comments/555");
}

#[tokio::test]
async fn setting_the_label_replaces_old_bounty_labels_and_tolerates_existing_ones() {
    let fake = Fake::start(|s| {
        if s.path.ends_with("/access_tokens") {
            return token_reply("t", Utc::now() + Duration::hours(1));
        }
        match (s.method.as_str(), s.path.as_str()) {
            ("GET", p) if p.contains("/issues/3/labels") => {
                (200, json!([{ "name": "bounty: 5 XLM" }, { "name": "bug" }, { "name": "bounty: 50 USDC" }]))
            }
            ("POST", "/repos/acme/widgets/labels") => (422, json!({ "message": "already_exists" })),
            _ => (200, json!([])),
        }
    })
    .await;
    fake.client().set_bounty_label(1, "acme/widgets", 3, "bounty: 50 USDC").await.unwrap();
    let seen = fake.seen();
    let deletes: Vec<_> = seen.iter().filter(|s| s.method == "DELETE").map(|s| s.path.as_str()).collect();
    assert_eq!(
        deletes,
        ["/repos/acme/widgets/issues/3/labels/bounty%3A%205%20XLM"],
        "only the stale bounty label goes; 'bug' and the current one stay"
    );
    let set = seen.iter().find(|s| s.method == "POST" && s.path.ends_with("/issues/3/labels")).unwrap();
    assert_eq!(set.body, json!({ "labels": ["bounty: 50 USDC"] }));
}

#[tokio::test]
async fn label_names_are_validated() {
    let fake = Fake::start(simple(json!([]))).await;
    for bad in ["bug", "bounty: ", "bounty: <script>", "bounty: a/b", &format!("bounty: {}", "x".repeat(41))] {
        assert!(fake.client().set_bounty_label(1, "a/b", 1, bad).await.is_err(), "{bad}");
    }
}

#[tokio::test]
async fn pull_files_paginate_up_to_three_pages() {
    let fake = Fake::start(|s| {
        if s.path.ends_with("/access_tokens") {
            return token_reply("t", Utc::now() + Duration::hours(1));
        }
        let page: usize = s.path.rsplit("page=").next().unwrap().parse().unwrap();
        let n = 100; // every page is full: only the three-page cap should stop the client
        (
            200,
            Value::Array(
                (0..n).map(|i| json!({ "filename": format!("f{page}-{i}"), "additions": 1, "deletions": 0, "patch": "@@" })).collect(),
            ),
        )
    })
    .await;
    let files = fake.client().get_pull_files(1, "acme/widgets", 8).await.unwrap();
    assert_eq!(files.len(), 300);
    assert_eq!(fake.seen().iter().filter(|s| s.path.contains("/files")).count(), 3);

    let short = Fake::start(|s| {
        if s.path.ends_with("/access_tokens") {
            token_reply("t", Utc::now() + Duration::hours(1))
        } else {
            (200, json!([{ "filename": "a" }]))
        }
    })
    .await;
    let files = short.client().get_pull_files(1, "acme/widgets", 8).await.unwrap();
    assert_eq!((files.len(), files[0].patch.clone()), (1, None));
    assert_eq!(short.seen().iter().filter(|s| s.path.contains("/files")).count(), 1);
}

#[tokio::test]
async fn ci_conclusion_is_derived_from_check_runs() {
    async fn conclude(runs: Value) -> CiConclusion {
        let fake = Fake::start(move |s| {
            if s.path.ends_with("/access_tokens") {
                token_reply("t", Utc::now() + Duration::hours(1))
            } else {
                (200, json!({ "check_runs": runs }))
            }
        })
        .await;
        fake.client().get_ci_conclusion(1, "a/b", "abcdef1").await.unwrap()
    }
    assert_eq!(conclude(json!([])).await, CiConclusion::None);
    assert_eq!(
        conclude(json!([{ "status": "completed", "conclusion": "success" }, { "status": "completed", "conclusion": "skipped" }])).await,
        CiConclusion::Success
    );
    assert_eq!(
        conclude(json!([{ "status": "completed", "conclusion": "success" }, { "status": "in_progress", "conclusion": null }])).await,
        CiConclusion::Pending
    );
    assert_eq!(
        conclude(json!([{ "status": "completed", "conclusion": "success" }, { "status": "completed", "conclusion": "failure" }])).await,
        CiConclusion::Failure
    );
    let fake = Fake::start(simple(json!({}))).await;
    assert!(fake.client().get_ci_conclusion(1, "a/b", "not-a-sha").await.is_err());
    assert!(fake.client().get_ci_conclusion(1, "a/b", "../..").await.is_err());
}

#[tokio::test]
async fn readme_excerpt_is_decoded_and_truncated() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let content = STANDARD.encode("# Widgets\nA very useful library.");
    let fake = Fake::start(move |s| {
        if s.path.ends_with("/access_tokens") {
            token_reply("t", Utc::now() + Duration::hours(1))
        } else {
            (200, json!({ "encoding": "base64", "content": content }))
        }
    })
    .await;
    assert_eq!(fake.client().get_readme_excerpt(1, "a/b", 9).await.unwrap(), "# Widgets");
    let missing =
        Fake::start(
            |s| if s.path.ends_with("/access_tokens") { token_reply("t", Utc::now() + Duration::hours(1)) } else { (404, json!({})) },
        )
        .await;
    assert_eq!(missing.client().get_readme_excerpt(1, "a/b", 100).await.unwrap(), "", "no README is not an error");
}

#[tokio::test]
async fn server_errors_are_transient_and_client_errors_are_not() {
    let fake =
        Fake::start(
            |s| if s.path.ends_with("/access_tokens") { token_reply("t", Utc::now() + Duration::hours(1)) } else { (502, json!({})) },
        )
        .await;
    assert!(fake.client().post_comment(1, "a/b", 1, "x").await.unwrap_err().is_transient());
    let fake =
        Fake::start(
            |s| if s.path.ends_with("/access_tokens") { token_reply("t", Utc::now() + Duration::hours(1)) } else { (403, json!({})) },
        )
        .await;
    assert!(!fake.client().post_comment(1, "a/b", 1, "x").await.unwrap_err().is_transient());
}

#[tokio::test]
async fn installations_and_repositories_list() {
    let fake = Fake::start(|s| {
        if s.path.ends_with("/access_tokens") {
            return token_reply("t", Utc::now() + Duration::hours(1));
        }
        if s.path.starts_with("/app/installations") {
            (200, json!([{ "id": 1, "account": { "login": "acme", "type": "Organization" }, "suspended_at": null }]))
        } else {
            (200, json!({ "total_count": 1, "repositories": [{ "id": 5, "full_name": "acme/widgets", "private": true }] }))
        }
    })
    .await;
    let c = fake.client();
    let i = c.list_installations().await.unwrap();
    assert_eq!((i[0].id, i[0].account.login.as_str(), i[0].suspended_at), (1, "acme", None));
    assert!(c.list_repositories(1).await.unwrap()[0].private);
}

// ---- sign-in ----

#[tokio::test]
async fn oauth_exchanges_the_code_with_the_pkce_verifier_and_reads_the_profile() {
    let fake = Fake::start(|s| match s.path.as_str() {
        "/login/oauth/access_token" => (200, json!({ "access_token": "gho_x" })),
        "/user" => (200, json!({ "id": 583231, "login": "octocat", "avatar_url": "https://a/b.png" })),
        _ => (404, json!({})),
    })
    .await;
    let c = OAuthClient::with_bases("cid", "csecret", &fake.url, &fake.url);
    let tok = c.exchange_code("the-code", "http://localhost:3001/auth/callback", "the-verifier").await.unwrap();
    assert_eq!(tok, "gho_x");
    let sent = fake.seen()[0].body.clone();
    assert_eq!(sent["code_verifier"], "the-verifier");
    assert_eq!(sent["client_id"], "cid");
    assert_eq!(sent["redirect_uri"], "http://localhost:3001/auth/callback");
    let u = c.fetch_user(&tok).await.unwrap();
    assert_eq!((u.id, u.login.as_str()), (583231, "octocat"));
    assert_eq!(fake.seen()[1].auth, "Bearer gho_x");
}

#[tokio::test]
async fn oauth_failures_are_distinguished() {
    let rejected = Fake::start(|_| (200, json!({ "error": "bad_verification_code" }))).await;
    let c = OAuthClient::with_bases("cid", "s", &rejected.url, &rejected.url);
    assert_eq!(c.exchange_code("x", "u", "v").await, Err(OAuthError::Rejected("bad_verification_code".into())));
    let broken = Fake::start(|_| (500, json!({}))).await;
    let c = OAuthClient::with_bases("cid", "s", &broken.url, &broken.url);
    assert!(c.fetch_user("t").await.is_err());
    let nonsense = Fake::start(|_| (200, json!({ "id": 0, "login": "" }))).await;
    assert!(OAuthClient::with_bases("c", "s", &nonsense.url, &nonsense.url).fetch_user("t").await.is_err());
    let dead = OAuthClient::with_bases("c", "s", "http://127.0.0.1:1", "http://127.0.0.1:1");
    assert_eq!(dead.exchange_code("x", "u", "v").await, Err(OAuthError::Unavailable));
}
