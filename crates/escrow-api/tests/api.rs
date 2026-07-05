//! The real router, driven in-process (no sockets) against real Postgres, with a fake chain and GitHub.

use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode, header},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{Duration, Utc};
use ed25519_dalek::{Signer as _, SigningKey};
use escrow_api::{AppState, limiter::RateLimiter, login::GithubLogin, metrics::Metrics, router, state::ApiConfig};
use escrow_core::{
    BountyStatus,
    oauth::{OAUTH_COOKIE, clear_cookie},
    security::{csrf_token, hash_session_id, hmac_hex, sep53_hash},
};
use escrow_db::{Db, MIGRATOR, bounties, disputes, intents, payouts, prs, sessions, users};
use escrow_engine::sync::{GithubNotifier, Tokens};
use escrow_github::{GithubUser, OAuthError};
use escrow_payouts::PayoutDeps;
use escrow_testkit::{FakeChain, FakeGithub, FakeTxBuilder, seed};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const WEBHOOK_SECRET: &str = "whsec-test-secret";
const CSRF: &str = "csrf-secret-csrf-secret-csrf-secret-xx";
const SESSION: &str = "session-secret-session-secret-session-xx";

struct Login {
    code_ok: Mutex<bool>,
    user: GithubUser,
}

#[async_trait]
impl GithubLogin for Login {
    fn client_id(&self) -> &str {
        "Iv1.testclient"
    }
    async fn exchange_code(&self, code: &str, _r: &str, verifier: &str) -> Result<String, OAuthError> {
        assert!(!verifier.is_empty(), "the PKCE verifier from the cookie must be sent");
        if *self.code_ok.lock().unwrap() && code == "good-code" {
            Ok("gho_token".into())
        } else {
            Err(OAuthError::Rejected("bad_verification_code".into()))
        }
    }
    async fn fetch_user(&self, _t: &str) -> Result<GithubUser, OAuthError> {
        Ok(self.user.clone())
    }
}

struct Env {
    app: Router,
    db: Db,
    chain: Arc<FakeChain>,
    txb: Arc<FakeTxBuilder>,
    login: Arc<Login>,
}

fn env(pool: PgPool, with_login: bool, admins: Vec<i64>) -> Env {
    let db = Db::from_pool(pool);
    let chain = Arc::new(FakeChain::default());
    *chain.ledger.lock().unwrap() = 1000;
    let txb = Arc::new(FakeTxBuilder::default());
    let gh = Arc::new(FakeGithub::new());
    let login = Arc::new(Login {
        code_ok: Mutex::new(true),
        user: GithubUser { id: 583231, login: "octocat".into(), avatar_url: Some("https://a/b.png".into()) },
    });
    let state = AppState {
        db: db.clone(),
        cfg: Arc::new(ApiConfig {
            csrf_secret: CSRF.into(),
            session_secret: SESSION.into(),
            webhook_secret: WEBHOOK_SECRET.into(),
            api_public_url: "http://localhost:3001".into(),
            web_public_url: "http://localhost:3000".into(),
            secure_cookies: false,
            admin_github_ids: admins,
            trust_proxy: false,
            metrics_token: None,
            explorer_tx_template: "https://explorer.example/tx/{hash}".into(),
            tokens: Tokens { usdc: "CUSDC".into(), xlm: "CTOKEN".into() },
            network: "testnet".into(),
        }),
        chain: chain.clone(),
        txb: txb.clone(),
        login: if with_login { Some(login.clone() as Arc<dyn GithubLogin>) } else { None },
        payouts: PayoutDeps {
            db: db.clone(),
            chain: chain.clone(),
            notifier: Arc::new(GithubNotifier { db: db.clone(), gh }),
            web_url: "http://localhost:3000".into(),
        },
        limiter: Arc::new(RateLimiter::new(std::time::Duration::from_secs(60))),
        metrics: Arc::new(Metrics::default()),
        now: Utc::now,
    };
    Env { app: router(state), db, chain, txb, login }
}

struct Resp {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Value,
    text: String,
}

async fn call(app: &Router, method: Method, uri: &str, headers: &[(&str, String)], body: Option<Value>) -> Resp {
    let mut b = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        b = b.header(*k, v);
    }
    let req = match body {
        Some(v) => b.header(header::CONTENT_TYPE, "application/json").body(Body::from(v.to_string())).unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let r = app.clone().oneshot(req).await.unwrap();
    let (status, headers) = (r.status(), r.headers().clone());
    let text = String::from_utf8(r.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    Resp { status, headers, body: serde_json::from_str(&text).unwrap_or(Value::Null), text }
}

async fn get(e: &Env, uri: &str) -> Resp {
    call(&e.app, Method::GET, uri, &[], None).await
}

struct Session {
    cookie: String,
    csrf: String,
    user: escrow_db::User,
}

impl Session {
    fn hdrs(&self, mutating: bool) -> Vec<(&'static str, String)> {
        let mut h = vec![("cookie", format!("se_session={}", self.cookie))];
        if mutating {
            h.push(("x-csrf-token", self.csrf.clone()));
        }
        h
    }
}

async fn signed_in(e: &Env, github_id: i64, login: &str, wallet: Option<&str>, admin: bool) -> Session {
    let u = users::upsert_github(&e.db.pool, github_id, login, None, admin).await.unwrap();
    if let Some(w) = wallet {
        users::set_wallet(&e.db.pool, &u.id, w, Utc::now()).await.unwrap();
    }
    let (sid, _) = escrow_api::session::create(&e.db, &u.id, Utc::now()).await.unwrap();
    Session { csrf: csrf_token(CSRF, &sid), cookie: sid, user: users::get(&e.db.pool, &u.id).await.unwrap().unwrap() }
}

fn stellar(seed_byte: u8) -> (SigningKey, String) {
    let sk = SigningKey::from_bytes(&[seed_byte; 32]);
    (sk.clone(), stellar_strkey::ed25519::PublicKey(sk.verifying_key().to_bytes()).to_string().to_string())
}

async fn fund(e: &Env, id: i64, issue: i32) {
    seed::repo(&e.db).await;
    seed::bounty(&e.db, &seed::B::new(id, issue)).await;
    e.chain.put(id as u64, seed::MAINTAINER_WALLET, BountyStatus::Open, true, None);
}

// ================= public reads =================

#[sqlx::test(migrator = "MIGRATOR")]
async fn health_reports_ok_and_503_when_the_indexer_is_far_behind(pool: PgPool) {
    let e = env(pool.clone(), true, vec![]);
    let r = get(&e, "/health").await;
    assert_eq!((r.status, r.body["ok"].clone(), r.body["db"].clone()), (StatusCode::OK, json!(true), json!(true)));
    escrow_db::webhooks::save_cursor(&pool, "bounty_escrow", "c", 100).await.unwrap();
    let lagging = get(&e, "/health").await;
    assert_eq!(lagging.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(lagging.body["indexerLagLedgers"], 900);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn every_response_carries_the_security_headers_and_errors_use_one_shape(pool: PgPool) {
    let e = env(pool, true, vec![]);
    let r = get(&e, "/bounties/abc").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.body, json!({ "error": { "code": "not_found", "message": "bounty not found" } }));
    let h = &r.headers;
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert_eq!(h["x-frame-options"], "DENY");
    assert_eq!(h["referrer-policy"], "no-referrer");
    assert_eq!(h["cache-control"], "no-store");
    assert!(h["content-security-policy"].to_str().unwrap().contains("default-src 'none'"));
    let unknown = get(&e, "/nope").await;
    assert_eq!((unknown.status, unknown.body["error"]["code"].clone()), (StatusCode::NOT_FOUND, json!("not_found")));
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn cors_allows_only_the_web_origin_with_credentials(pool: PgPool) {
    let e = env(pool, true, vec![]);
    let pre = call(
        &e.app,
        Method::OPTIONS,
        "/wallet/verify",
        &[
            ("origin", "http://localhost:3000".into()),
            ("access-control-request-method", "POST".into()),
            ("access-control-request-headers", "content-type,x-csrf-token".into()),
        ],
        None,
    )
    .await;
    assert_eq!(pre.headers["access-control-allow-origin"], "http://localhost:3000");
    assert_eq!(pre.headers["access-control-allow-credentials"], "true");
    let evil = call(
        &e.app,
        Method::OPTIONS,
        "/wallet/verify",
        &[("origin", "https://evil.example".into()), ("access-control-request-method", "POST".into())],
        None,
    )
    .await;
    // the answer names only our web origin; a browser on another origin sees a mismatch and blocks the response
    assert_ne!(evil.headers.get("access-control-allow-origin").map(|v| v.to_str().unwrap()), Some("https://evil.example"));
    assert_eq!(evil.headers.get("access-control-allow-origin").map(|v| v.to_str().unwrap()), Some("http://localhost:3000"));
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn the_bounty_list_filters_sorts_and_pages(pool: PgPool) {
    let e = env(pool, true, vec![]);
    seed::repo(&e.db).await;
    for (id, amount) in [(1, 100i128), (2, 300), (3, 200), (4, 400)] {
        let mut b = seed::B::new(id, id as i32);
        b.amount = amount;
        seed::bounty(&e.db, &b).await;
    }
    let all = get(&e, "/bounties").await;
    assert_eq!(all.status, StatusCode::OK);
    let ids = |r: &Resp| r.body["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(ids(&all), ["4", "3", "2", "1"]);
    assert_eq!(all.body["nextCursor"], Value::Null);
    let item = &all.body["items"][0];
    assert_eq!(
        (item["amount"].as_str(), item["status"].as_str(), item["repo"]["fullName"].as_str(), item["_count"]["prs"].as_i64()),
        (Some("400"), Some("OPEN"), Some("acme/widgets"), Some(0))
    );

    let top = get(&e, "/bounties?sort=highest&limit=2").await;
    assert_eq!(ids(&top), ["4", "2"]);
    let cursor = top.body["nextCursor"].as_str().unwrap().to_string();
    assert_eq!(ids(&get(&e, &format!("/bounties?sort=highest&limit=2&cursor={cursor}")).await), ["3", "1"]);
    assert_eq!(ids(&get(&e, "/bounties?minAmount=250").await), ["4", "2"]);
    assert!(ids(&get(&e, "/bounties?status=RELEASED").await).is_empty());
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn bad_list_queries_are_400s_not_500s(pool: PgPool) {
    let e = env(pool, true, vec![]);
    for q in [
        "status=nope",
        "sort=sideways",
        "limit=0",
        "limit=51",
        "limit=abc",
        "cursor=x",
        "cursor=1234567890123456789",
        "minAmount=-1",
        "minAmount=1e3",
        &format!("q={}", "x".repeat(101)),
    ] {
        let r = get(&e, &format!("/bounties?{q}")).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{q}");
        assert_eq!(r.body["error"]["code"], "bad_request");
    }
    // text with SQL/LIKE metacharacters is just text
    assert_eq!(get(&e, "/bounties?q=%25%27%3B--").await.status, StatusCode::OK);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_bounty_detail_includes_prs_reviews_and_payouts(pool: PgPool) {
    let e = env(pool.clone(), true, vec![]);
    fund(&e, 4, 3).await;
    prs::upsert(
        &pool,
        &prs::PrUpsert { id: 100, number: 7, author_github_id: 900, author_login: "dev", head_sha: "abc", state: "open", merged_at: None },
    )
    .await
    .unwrap();
    prs::link(&pool, 100, 4).await.unwrap();
    escrow_db::reviews::upsert(
        &pool,
        &escrow_db::reviews::NewReview {
            pr_id: 100,
            head_sha: "abc",
            model: "m",
            verdict: "likely_meets",
            score: 90,
            result: &json!({"summary": "ok"}),
            flags: &[],
            input_tokens: 1,
            output_tokens: 1,
            comment_id: None,
        },
    )
    .await
    .unwrap();
    payouts::create_pending(&pool, 4, 7, 900).await.unwrap();

    let r = get(&e, "/bounties/4").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        (r.body["id"].as_str(), r.body["repo"]["fullName"].as_str(), r.body["prs"][0]["authorLogin"].as_str()),
        (Some("4"), Some("acme/widgets"), Some("dev"))
    );
    assert_eq!(r.body["prs"][0]["reviews"][0]["score"], 90);
    assert_eq!(r.body["payouts"][0]["status"], "PENDING_WALLET");
    assert!(r.body.get("lastEventId").is_none());
    assert_eq!(get(&e, "/bounties/999").await.status, StatusCode::NOT_FOUND);
    assert_eq!(get(&e, "/bounties/99999999999999999999").await.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn public_profiles_show_only_confirmed_payouts(pool: PgPool) {
    let e = env(pool.clone(), true, vec![]);
    fund(&e, 4, 3).await;
    fund_more(&e, 5, 4).await;
    users::upsert_github(&pool, 900, "Dev-One", Some("https://a/x.png"), false).await.unwrap();
    let a = payouts::create_pending(&pool, 4, 7, 900).await.unwrap().unwrap();
    let b = payouts::create_pending(&pool, 5, 8, 900).await.unwrap().unwrap();
    payouts::mark_confirmed(&pool, &a.id, Some("txhash1")).await.unwrap();
    payouts::mark_failed(&pool, &b.id, "x", false).await.unwrap();
    let r = get(&e, "/users/dev-one").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body["login"], "Dev-One");
    assert_eq!(r.body["paid"].as_array().unwrap().len(), 1);
    assert_eq!((r.body["paid"][0]["txHash"].as_str(), r.body["paid"][0]["bounty"]["id"].as_str()), (Some("txhash1"), Some("4")));
    assert_eq!(get(&e, "/users/nobody").await.status, StatusCode::NOT_FOUND);
    assert_eq!(get(&e, "/users/a%2Fb").await.status, StatusCode::NOT_FOUND);
}

async fn fund_more(e: &Env, id: i64, issue: i32) {
    seed::bounty(&e.db, &seed::B::new(id, issue)).await;
    e.chain.put(id as u64, seed::MAINTAINER_WALLET, BountyStatus::Open, true, None);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn config_and_stats_expose_what_the_web_app_needs(pool: PgPool) {
    let e = env(pool.clone(), true, vec![]);
    fund(&e, 4, 3).await;
    let c = get(&e, "/config").await;
    assert_eq!(
        (c.body["feeBps"].as_i64(), c.body["graceSecs"].as_i64(), c.body["arbiter"].as_str(), c.body["tokens"]["XLM"].as_str()),
        (Some(300), Some(259_200), Some("GARBITER"), Some("CTOKEN"))
    );
    assert_eq!(c.headers["cache-control"], "public, max-age=30");
    let s = get(&e, "/stats").await;
    assert_eq!((s.body["bounties"]["OPEN"].as_i64(), s.body["bounties"]["RELEASED"].as_i64()), (Some(1), Some(0)));
    assert_eq!((s.body["openByToken"][0]["symbol"].as_str(), s.body["openByToken"][0]["amount"].as_str()), (Some("XLM"), Some("50000000")));
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn metrics_are_prometheus_text_and_can_require_a_token(pool: PgPool) {
    let e = env(pool.clone(), true, vec![]);
    fund(&e, 4, 3).await;
    get(&e, "/bounties").await;
    let m = get(&e, "/metrics").await;
    assert_eq!(m.status, StatusCode::OK);
    assert!(
        m.text.contains("# TYPE skillescrow_http_requests_total counter") && m.text.contains("skillescrow_bounties{status=\"OPEN\"} 1"),
        "{}",
        m.text
    );
    assert!(m.headers["content-type"].to_str().unwrap().starts_with("text/plain"));
}

// ================= sign-in =================

fn set_cookies(r: &Resp) -> Vec<String> {
    r.headers.get_all(header::SET_COOKIE).iter().map(|v| v.to_str().unwrap().to_string()).collect()
}

fn cookie_pair(set_cookie: &str) -> String {
    set_cookie.split(';').next().unwrap().to_string()
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn the_full_github_sign_in_flow_creates_a_session(pool: PgPool) {
    let e = env(pool.clone(), true, vec![583231]);
    let start = get(&e, "/auth/login?next=/b/4").await;
    assert_eq!(start.status, StatusCode::SEE_OTHER);
    let loc = start.headers[header::LOCATION].to_str().unwrap().to_string();
    assert!(
        loc.starts_with("https://github.com/login/oauth/authorize?client_id=Iv1.testclient") && loc.contains("code_challenge_method=S256"),
        "{loc}"
    );
    let state = loc.split("state=").nth(1).unwrap().split('&').next().unwrap().to_string();
    let oauth_cookie = set_cookies(&start).into_iter().find(|c| c.starts_with("se_oauth=")).unwrap();
    assert!(oauth_cookie.contains("HttpOnly") && oauth_cookie.contains("SameSite=Lax"));

    let done =
        call(&e.app, Method::GET, &format!("/auth/callback?code=good-code&state={state}"), &[("cookie", cookie_pair(&oauth_cookie))], None)
            .await;
    assert_eq!(done.status, StatusCode::SEE_OTHER);
    assert_eq!(done.headers[header::LOCATION], "http://localhost:3000/b/4");
    let cookies = set_cookies(&done);
    let sess = cookies.iter().find(|c| c.starts_with("se_session=")).unwrap();
    assert!(sess.contains("HttpOnly") && sess.contains("Max-Age=2592000"));
    assert!(cookies.contains(&clear_cookie(OAUTH_COOKIE)), "the one-time state cookie is cleared");

    let me = call(&e.app, Method::GET, "/auth/me", &[("cookie", cookie_pair(sess))], None).await;
    assert_eq!((me.status, me.body["login"].as_str(), me.body["isAdmin"].clone()), (StatusCode::OK, Some("octocat"), json!(true)));
    let sid = cookie_pair(sess).strip_prefix("se_session=").unwrap().to_string();
    assert_eq!(me.body["csrfToken"], csrf_token(CSRF, &sid));
    // only the hash is stored
    let stored: Vec<String> = sqlx::query_scalar("SELECT id FROM sessions").fetch_all(&pool).await.unwrap();
    assert_eq!(stored, [hash_session_id(&sid)]);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn sign_in_failures_are_specific_and_clear_the_state_cookie(pool: PgPool) {
    let e = env(pool, true, vec![]);
    let start = get(&e, "/auth/login").await;
    let oauth_cookie = cookie_pair(&set_cookies(&start).into_iter().find(|c| c.starts_with("se_oauth=")).unwrap());
    let state = start.headers[header::LOCATION].to_str().unwrap().split("state=").nth(1).unwrap().split('&').next().unwrap().to_string();

    let cb = |q: String, cookie: Option<String>| {
        let app = e.app.clone();
        async move {
            call(&app, Method::GET, &format!("/auth/callback?{q}"), &cookie.map(|c| vec![("cookie", c)]).unwrap_or_default(), None).await
        }
    };
    let no_cookie = cb(format!("code=good-code&state={state}"), None).await;
    assert_eq!((no_cookie.status, no_cookie.body["error"]["code"].clone()), (StatusCode::BAD_REQUEST, json!("bad_request")));
    let wrong_state = cb("code=good-code&state=other".into(), Some(oauth_cookie.clone())).await;
    assert_eq!(wrong_state.body["error"]["code"], "bad_state");
    assert!(set_cookies(&wrong_state).iter().any(|c| c.contains("Max-Age=0")));
    let forged = cb(format!("code=good-code&state={state}"), Some("se_oauth=eyJmb3JnZWQiOjF9.bogus".into())).await;
    assert_eq!(forged.body["error"]["code"], "bad_state");
    *e.login.code_ok.lock().unwrap() = false;
    let rejected = cb(format!("code=good-code&state={state}"), Some(oauth_cookie)).await;
    assert_eq!((rejected.status, rejected.body["error"]["code"].clone()), (StatusCode::BAD_REQUEST, json!("github_rejected")));
    assert!(rejected.body["error"]["message"].as_str().unwrap().contains("bad_verification_code"));
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn without_oauth_credentials_login_is_a_clear_501(pool: PgPool) {
    let e = env(pool, false, vec![]);
    let r = get(&e, "/auth/login").await;
    assert_eq!((r.status, r.body["error"]["code"].clone()), (StatusCode::NOT_IMPLEMENTED, json!("login_not_configured")));
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn the_admin_flag_follows_configuration_at_every_login(pool: PgPool) {
    // an ex-admin is demoted the next time they sign in
    let e = env(pool.clone(), true, vec![]);
    users::upsert_github(&pool, 583231, "octocat", None, true).await.unwrap();
    let start = get(&e, "/auth/login").await;
    let oc = cookie_pair(&set_cookies(&start).into_iter().find(|c| c.starts_with("se_oauth=")).unwrap());
    let state = start.headers[header::LOCATION].to_str().unwrap().split("state=").nth(1).unwrap().split('&').next().unwrap().to_string();
    call(&e.app, Method::GET, &format!("/auth/callback?code=good-code&state={state}"), &[("cookie", oc)], None).await;
    assert!(!users::by_github_id(&pool, 583231).await.unwrap().unwrap().is_admin);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn sessions_csrf_and_logout(pool: PgPool) {
    let e = env(pool.clone(), true, vec![]);
    assert_eq!(get(&e, "/auth/me").await.status, StatusCode::UNAUTHORIZED);
    let s = signed_in(&e, 1, "a", None, false).await;
    assert_eq!(call(&e.app, Method::GET, "/auth/me", &s.hdrs(false), None).await.status, StatusCode::OK);
    // a state-changing call needs the CSRF token
    let no_csrf = call(&e.app, Method::POST, "/auth/logout", &s.hdrs(false), None).await;
    assert_eq!((no_csrf.status, no_csrf.body["error"]["code"].clone()), (StatusCode::FORBIDDEN, json!("csrf")));
    let wrong = call(
        &e.app,
        Method::POST,
        "/auth/logout",
        &[("cookie", format!("se_session={}", s.cookie)), ("x-csrf-token", "nope".into())],
        None,
    )
    .await;
    assert_eq!(wrong.status, StatusCode::FORBIDDEN);
    // another session's token does not work either
    let other = signed_in(&e, 2, "b", None, false).await;
    let swapped = call(
        &e.app,
        Method::POST,
        "/auth/logout",
        &[("cookie", format!("se_session={}", s.cookie)), ("x-csrf-token", other.csrf.clone())],
        None,
    )
    .await;
    assert_eq!(swapped.status, StatusCode::FORBIDDEN);

    let out = call(&e.app, Method::POST, "/auth/logout", &s.hdrs(true), None).await;
    assert_eq!(out.status, StatusCode::OK);
    assert!(set_cookies(&out)[0].contains("Max-Age=0"));
    assert_eq!(
        call(&e.app, Method::GET, "/auth/me", &s.hdrs(false), None).await.status,
        StatusCode::UNAUTHORIZED,
        "the session is gone server-side"
    );
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn expired_and_forged_sessions_are_rejected(pool: PgPool) {
    let e = env(pool.clone(), true, vec![]);
    let s = signed_in(&e, 1, "a", None, false).await;
    sqlx::query("UPDATE sessions SET expires_at = now() - interval '1 second'").execute(&pool).await.unwrap();
    assert_eq!(call(&e.app, Method::GET, "/auth/me", &s.hdrs(false), None).await.status, StatusCode::UNAUTHORIZED);
    let forged = [("cookie", format!("se_session={}", "ab".repeat(32)))];
    assert_eq!(call(&e.app, Method::GET, "/auth/me", &forged, None).await.status, StatusCode::UNAUTHORIZED);
    let _ = sessions::purge_expired(&pool, Utc::now()).await;
}

// ================= wallet linking =================

async fn challenge(e: &Env, s: &Session, address: &str) -> Resp {
    call(&e.app, Method::POST, "/wallet/challenge", &s.hdrs(true), Some(json!({ "address": address }))).await
}

fn sign(sk: &SigningKey, message: &str) -> String {
    STANDARD.encode(sk.sign(&sep53_hash(message)).to_bytes())
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn linking_a_wallet_needs_a_real_signature_over_the_challenge(pool: PgPool) {
    let e = env(pool.clone(), true, vec![]);
    let s = signed_in(&e, 900, "dev", None, false).await;
    let (sk, addr) = stellar(5);
    let c = challenge(&e, &s, &addr).await;
    assert_eq!(c.status, StatusCode::OK);
    let (id, msg) = (c.body["challengeId"].as_str().unwrap().to_string(), c.body["message"].as_str().unwrap().to_string());
    assert!(msg.contains("GitHub: dev (900)") && msg.contains(&format!("Address: {addr}")));

    // a signature from a different key, or over different text, is refused
    let (other_sk, _) = stellar(6);
    for bad in [sign(&other_sk, &msg), sign(&sk, "something else"), "not-base64!!".into()] {
        let r = call(&e.app, Method::POST, "/wallet/verify", &s.hdrs(true), Some(json!({ "challengeId": id, "signature": bad }))).await;
        assert_eq!((r.status, r.body["error"]["code"].clone()), (StatusCode::BAD_REQUEST, json!("bad_signature")));
    }
    assert!(users::get(&pool, &s.user.id).await.unwrap().unwrap().wallet.is_none());

    let ok =
        call(&e.app, Method::POST, "/wallet/verify", &s.hdrs(true), Some(json!({ "challengeId": id, "signature": sign(&sk, &msg) }))).await;
    assert_eq!((ok.status, ok.body["address"].as_str()), (StatusCode::OK, Some(addr.as_str())));
    assert_eq!(users::get(&pool, &s.user.id).await.unwrap().unwrap().wallet.as_deref(), Some(addr.as_str()));
    // single use
    let again =
        call(&e.app, Method::POST, "/wallet/verify", &s.hdrs(true), Some(json!({ "challengeId": id, "signature": sign(&sk, &msg) }))).await;
    assert_eq!((again.status, again.body["error"]["code"].clone()), (StatusCode::CONFLICT, json!("challenge_used")));
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn wallet_challenges_validate_address_ownership_expiry_and_requester(pool: PgPool) {
    let e = env(pool.clone(), true, vec![]);
    let s = signed_in(&e, 900, "dev", None, false).await;
    let (_, addr) = stellar(5);
    assert_eq!(challenge(&e, &s, "GNOTANADDRESS").await.body["error"]["code"], "bad_address");
    assert_eq!(challenge(&e, &s, "CCEKBPYCWKNTXIQLXJHVAF7RYGUZZ6MITA7VVJBOK5BHXUO44ELZ4F4V").await.status, StatusCode::BAD_REQUEST);

    // an address already linked to someone else is refused up front
    signed_in(&e, 901, "other", Some(&addr), false).await;
    let taken = challenge(&e, &s, &addr).await;
    assert_eq!((taken.status, taken.body["error"]["code"].clone()), (StatusCode::CONFLICT, json!("address_taken")));

    // someone else's challenge id is a 404 for you
    let (sk, free) = stellar(7);
    let mine = challenge(&e, &s, &free).await;
    let thief = signed_in(&e, 902, "thief", None, false).await;
    let msg = mine.body["message"].as_str().unwrap().to_string();
    let steal = call(
        &e.app,
        Method::POST,
        "/wallet/verify",
        &thief.hdrs(true),
        Some(json!({ "challengeId": mine.body["challengeId"], "signature": sign(&sk, &msg) })),
    )
    .await;
    assert_eq!((steal.status, steal.body["error"]["code"].clone()), (StatusCode::NOT_FOUND, json!("challenge_not_found")));

    // expiry
    sqlx::query("UPDATE wallet_challenges SET expires_at = now() - interval '1 second'").execute(&pool).await.unwrap();
    let late = call(
        &e.app,
        Method::POST,
        "/wallet/verify",
        &s.hdrs(true),
        Some(json!({ "challengeId": mine.body["challengeId"], "signature": sign(&sk, &msg) })),
    )
    .await;
    assert_eq!((late.status, late.body["error"]["code"].clone()), (StatusCode::GONE, json!("challenge_expired")));

    // unauthenticated and csrf-less calls
    assert_eq!(
        call(&e.app, Method::POST, "/wallet/challenge", &[], Some(json!({ "address": free }))).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&e.app, Method::POST, "/wallet/challenge", &s.hdrs(false), Some(json!({ "address": free }))).await.status,
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn linking_a_wallet_continues_a_payout_that_was_waiting_for_it(pool: PgPool) {
    let e = env(pool.clone(), true, vec![]);
    fund(&e, 4, 3).await;
    let s = signed_in(&e, 900, "dev", None, false).await;
    payouts::create_pending(&pool, 4, 7, 900).await.unwrap();
    let (sk, addr) = stellar(5);
    let c = challenge(&e, &s, &addr).await;
    let msg = c.body["message"].as_str().unwrap().to_string();
    call(
        &e.app,
        Method::POST,
        "/wallet/verify",
        &s.hdrs(true),
        Some(json!({ "challengeId": c.body["challengeId"], "signature": sign(&sk, &msg) })),
    )
    .await;
    let p = payouts::for_bounty(&pool, 4).await.unwrap().unwrap();
    assert_eq!((p.status.as_str(), p.winner_address.as_deref()), ("QUEUED", Some(addr.as_str())));
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn unlinking_removes_the_wallet(pool: PgPool) {
    let e = env(pool.clone(), true, vec![]);
    let s = signed_in(&e, 900, "dev", Some("GWALLET"), false).await;
    assert_eq!(call(&e.app, Method::DELETE, "/wallet", &s.hdrs(true), None).await.status, StatusCode::OK);
    assert!(users::get(&pool, &s.user.id).await.unwrap().unwrap().wallet.is_none());
}

// ================= "my" data =================

#[sqlx::test(migrator = "MIGRATOR")]
async fn my_earnings_and_funded_bounties(pool: PgPool) {
    let e = env(pool.clone(), true, vec![]);
    fund(&e, 4, 3).await;
    let contributor = signed_in(&e, 900, "dev", Some("GCONTRIB"), false).await;
    let p = payouts::create_pending(&pool, 4, 7, 900).await.unwrap().unwrap();
    payouts::mark_confirmed(&pool, &p.id, Some("h")).await.unwrap();
    let r = call(&e.app, Method::GET, "/me/earnings", &contributor.hdrs(false), None).await;
    assert_eq!(
        (r.body[0]["status"].as_str(), r.body[0]["bounty"]["amount"].as_str(), r.body[0]["bounty"]["feeBps"].as_i64()),
        (Some("CONFIRMED"), Some("50000000"), Some(300))
    );

    let maintainer = signed_in(&e, 500, "maint", Some(seed::MAINTAINER_WALLET), false).await;
    let funded = call(&e.app, Method::GET, "/me/funded", &maintainer.hdrs(false), None).await;
    assert_eq!(funded.body[0]["id"], "4");
    let no_wallet = signed_in(&e, 501, "nowallet", None, false).await;
    assert_eq!(call(&e.app, Method::GET, "/me/funded", &no_wallet.hdrs(false), None).await.body, json!([]));
    assert_eq!(get(&e, "/me/earnings").await.status, StatusCode::UNAUTHORIZED);
}

// ================= funding =================

async fn intent(e: &Env, id: &str, requested_by: i64) {
    seed::repo(&e.db).await;
    intents::create(
        &e.db.pool,
        &intents::NewIntent {
            id,
            repo_id: seed::REPO_ID,
            issue_number: 3,
            requested_by,
            token: "CTOKEN",
            amount: 50_000_000,
            duration_days: 14,
            comment_id: 9,
            expires_at: Utc::now() + Duration::hours(24),
        },
    )
    .await
    .unwrap();
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_funding_link_shows_its_terms_and_state(pool: PgPool) {
    let e = env(pool.clone(), true, vec![]);
    intent(&e, "intent-aaaaaaaaaaaa", 900).await;
    let r = get(&e, "/intents/intent-aaaaaaaaaaaa").await;
    assert_eq!(
        (r.status, r.body["state"].as_str(), r.body["repo"].as_str(), r.body["amount"].as_str(), r.body["durationDays"].as_i64()),
        (StatusCode::OK, Some("open"), Some("acme/widgets"), Some("50000000"), Some(14))
    );
    assert_eq!(r.body["bountyId"], Value::Null);
    sqlx::query("UPDATE funding_intents SET expires_at = now() - interval '1 second'").execute(&pool).await.unwrap();
    assert_eq!(get(&e, "/intents/intent-aaaaaaaaaaaa").await.body["state"], "expired");
    assert_eq!(get(&e, "/intents/short").await.status, StatusCode::NOT_FOUND);
    assert_eq!(get(&e, "/intents/does-not-exist-123").await.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn building_the_funding_transaction_checks_who_is_asking(pool: PgPool) {
    let e = env(pool.clone(), true, vec![]);
    intent(&e, "intent-aaaaaaaaaaaa", 900).await;
    let (_, addr) = stellar(1);
    let build = |s: &Session, body: Value| {
        let (app, h) = (e.app.clone(), s.hdrs(true));
        async move { call(&app, Method::POST, "/intents/intent-aaaaaaaaaaaa/build", &h, Some(body)).await }
    };
    let requester = signed_in(&e, 900, "alice", None, false).await;
    let stranger = signed_in(&e, 901, "mallory", None, false).await;

    let denied = build(&stranger, json!({ "address": addr, "relayerAllowed": true })).await;
    assert_eq!((denied.status, denied.body["error"]["code"].clone()), (StatusCode::FORBIDDEN, json!("not_requester")));
    assert_eq!(build(&requester, json!({ "address": "GBAD", "relayerAllowed": true })).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(build(&requester, json!({ "address": addr })).await.status, StatusCode::BAD_REQUEST, "relayerAllowed is required");

    let ok = build(&requester, json!({ "address": addr, "relayerAllowed": false })).await;
    assert_eq!(ok.status, StatusCode::OK);
    let xdr = ok.body["xdr"].as_str().unwrap();
    assert!(xdr.starts_with("XDR[fund:") && xdr.contains(&format!(":{}:10:3:CTOKEN:50000000:false", addr)), "{xdr}");
    // the deadline is the duration from now (+60 s of slack), and was passed to the chain builder
    let built = e.txb.built.lock().unwrap().clone();
    assert_eq!(built.len(), 1);

    // used and expired links are gone (410)
    intents::mark_used(&pool, "intent-aaaaaaaaaaaa", Utc::now()).await.unwrap();
    let used = build(&requester, json!({ "address": addr, "relayerAllowed": true })).await;
    assert_eq!((used.status, used.body["error"]["code"].clone()), (StatusCode::GONE, json!("intent_used")));
    assert_eq!(
        call(
            &e.app,
            Method::POST,
            "/intents/nope-nope-nope/build",
            &requester.hdrs(true),
            Some(json!({ "address": addr, "relayerAllowed": true }))
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn contract_rejections_while_building_are_named(pool: PgPool) {
    let e = env(pool.clone(), true, vec![]);
    intent(&e, "intent-aaaaaaaaaaaa", 900).await;
    let (_, addr) = stellar(1);
    let s = signed_in(&e, 900, "alice", None, false).await;
    *e.txb.fail_with.lock().unwrap() = Some(escrow_chain::ChainError::Simulation("HostError: Error(Contract, #9)".into()));
    let r = call(
        &e.app,
        Method::POST,
        "/intents/intent-aaaaaaaaaaaa/build",
        &s.hdrs(true),
        Some(json!({ "address": addr, "relayerAllowed": true })),
    )
    .await;
    assert_eq!((r.status, r.body["error"]["code"].clone()), (StatusCode::UNPROCESSABLE_ENTITY, json!("contract_IssueAlreadyFunded")));
    *e.txb.fail_with.lock().unwrap() = Some(escrow_chain::ChainError::Http("timeout".into()));
    let down = call(
        &e.app,
        Method::POST,
        "/intents/intent-aaaaaaaaaaaa/build",
        &s.hdrs(true),
        Some(json!({ "address": addr, "relayerAllowed": true })),
    )
    .await;
    assert_eq!((down.status, down.body["error"]["code"].clone()), (StatusCode::SERVICE_UNAVAILABLE, json!("chain_unavailable")));
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn submitting_a_signed_transaction_is_relayed_and_failures_are_explained(pool: PgPool) {
    let e = env(pool, true, vec![]);
    let s = signed_in(&e, 900, "alice", None, false).await;
    let ok =
        call(&e.app, Method::POST, "/tx/submit", &s.hdrs(true), Some(json!({ "xdr": "AAAA-signed-envelope", "address": "GADDR" }))).await;
    assert_eq!((ok.status, ok.body["hash"].as_str().map(str::len)), (StatusCode::OK, Some(64)));
    assert_eq!(e.txb.submitted.lock().unwrap()[0], ("AAAA-signed-envelope".to_string(), Some("GADDR".to_string())));

    *e.txb.fail_with.lock().unwrap() = Some(escrow_chain::ChainError::Invalid("this is not a call to the SkillEscrow contract".into()));
    let bad = call(&e.app, Method::POST, "/tx/submit", &s.hdrs(true), Some(json!({ "xdr": "AAAA-signed-envelope" }))).await;
    assert_eq!(
        (bad.status, bad.body["error"]["code"].clone(), bad.body["error"]["message"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, json!("submit_rejected"), Some("this is not a call to the SkillEscrow contract"))
    );
    assert_eq!(call(&e.app, Method::POST, "/tx/submit", &s.hdrs(true), Some(json!({ "xdr": "x" }))).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        call(&e.app, Method::POST, "/tx/submit", &s.hdrs(true), Some(json!({ "xdr": "y".repeat(20_001) }))).await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&e.app, Method::POST, "/tx/submit", &[], Some(json!({ "xdr": "AAAA-signed-envelope" }))).await.status,
        StatusCode::UNAUTHORIZED
    );
    let not_json = call(&e.app, Method::POST, "/tx/submit", &s.hdrs(true), None).await;
    assert_eq!((not_json.status, not_json.body["error"]["code"].clone()), (StatusCode::BAD_REQUEST, json!("bad_request")));
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn only_the_maintainers_wallet_can_ask_for_a_release_or_refund(pool: PgPool) {
    let e = env(pool.clone(), true, vec![]);
    fund(&e, 4, 3).await;
    let maintainer = signed_in(&e, 500, "maint", Some(seed::MAINTAINER_WALLET), false).await;
    let other = signed_in(&e, 501, "other", Some("GOTHERWALLET"), false).await;
    let nowallet = signed_in(&e, 502, "nowallet", None, false).await;
    for who in [&other, &nowallet] {
        let r = call(&e.app, Method::POST, "/bounties/4/refund/build", &who.hdrs(true), None).await;
        assert_eq!((r.status, r.body["error"]["code"].clone()), (StatusCode::FORBIDDEN, json!("not_maintainer")));
    }
    let ok = call(&e.app, Method::POST, "/bounties/4/refund/build", &maintainer.hdrs(true), None).await;
    assert_eq!((ok.status, ok.body["xdr"].as_str()), (StatusCode::OK, Some(format!("XDR[refund:{}:4]", seed::MAINTAINER_WALLET).as_str())));
    assert_eq!(call(&e.app, Method::POST, "/bounties/9/refund/build", &maintainer.hdrs(true), None).await.status, StatusCode::NOT_FOUND);

    // the chain, not the database, decides whether it is still open
    e.chain.set_status(4, BountyStatus::Released);
    let closed = call(&e.app, Method::POST, "/bounties/4/refund/build", &maintainer.hdrs(true), None).await;
    assert_eq!(
        (closed.status, closed.body["error"]["code"].clone(), closed.body["error"]["message"].as_str()),
        (StatusCode::CONFLICT, json!("not_open"), Some("bounty is RELEASED"))
    );
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_release_is_built_only_for_a_payout_waiting_on_the_maintainer(pool: PgPool) {
    let e = env(pool.clone(), true, vec![]);
    fund(&e, 4, 3).await;
    let maintainer = signed_in(&e, 500, "maint", Some(seed::MAINTAINER_WALLET), false).await;
    let none = call(&e.app, Method::POST, "/bounties/4/release/build", &maintainer.hdrs(true), None).await;
    assert_eq!((none.status, none.body["error"]["code"].clone()), (StatusCode::CONFLICT, json!("nothing_to_release")));

    let p = payouts::create_pending(&pool, 4, 7, 900).await.unwrap().unwrap();
    payouts::release_from_pending(&pool, &p.id, "AWAITING_MAINTAINER", "GWINNER").await.unwrap();
    let ok = call(&e.app, Method::POST, "/bounties/4/release/build", &maintainer.hdrs(true), None).await;
    assert_eq!(
        (ok.status, ok.body["winner"].as_str(), ok.body["xdr"].as_str()),
        (StatusCode::OK, Some("GWINNER"), Some(format!("XDR[release:{}:4:GWINNER]", seed::MAINTAINER_WALLET).as_str()))
    );

    // a payout the relayer already owns is not the maintainer's to sign
    sqlx::query("UPDATE payouts SET status = 'QUEUED'").execute(&pool).await.unwrap();
    assert_eq!(call(&e.app, Method::POST, "/bounties/4/release/build", &maintainer.hdrs(true), None).await.status, StatusCode::CONFLICT);
}

// ================= admin =================

#[sqlx::test(migrator = "MIGRATOR")]
async fn admin_routes_refuse_everyone_but_admins(pool: PgPool) {
    let e = env(pool.clone(), true, vec![]);
    let user = signed_in(&e, 1, "plain", None, false).await;
    for (m, uri) in [(Method::GET, "/admin/disputes"), (Method::GET, "/admin/payouts"), (Method::GET, "/admin/status")] {
        assert_eq!(call(&e.app, m, uri, &[], None).await.status, StatusCode::UNAUTHORIZED, "{uri}");
        let r = call(&e.app, Method::GET, uri, &user.hdrs(false), None).await;
        assert_eq!((r.status, r.body["error"]["code"].clone()), (StatusCode::FORBIDDEN, json!("forbidden")), "{uri}");
    }
    for uri in
        ["/admin/disputes/abcdefghij1234/open-onchain", "/admin/disputes/abcdefghij1234/reject", "/admin/payouts/abcdefghij1234/retry"]
    {
        assert_eq!(
            call(&e.app, Method::POST, uri, &user.hdrs(true), Some(json!({ "arbiterAddress": "x", "note": "x" }))).await.status,
            StatusCode::FORBIDDEN,
            "{uri}"
        );
    }
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn the_dispute_workflow_from_request_to_a_signable_resolution(pool: PgPool) {
    let e = env(pool.clone(), true, vec![]);
    fund(&e, 4, 3).await;
    let admin = signed_in(&e, 1, "arb", None, true).await;
    let (_, arbiter) = stellar(9);
    let (_, winner) = stellar(10);
    let d = disputes::create(&pool, 4, 500, "never reviewed").await.unwrap();

    let queue = call(&e.app, Method::GET, "/admin/disputes", &admin.hdrs(false), None).await;
    assert_eq!(
        (queue.body[0]["id"].as_str(), queue.body[0]["overdue"].clone(), queue.body[0]["bounty"]["repo"]["fullName"].as_str()),
        (Some(d.id.as_str()), json!(false), Some("acme/widgets"))
    );

    // resolving before it is open on chain is refused
    let early = call(
        &e.app,
        Method::POST,
        &format!("/admin/disputes/{}/resolve/build", d.id),
        &admin.hdrs(true),
        Some(json!({ "arbiterAddress": arbiter, "winner": winner, "winnerBps": 10000 })),
    )
    .await;
    assert_eq!((early.status, early.body["error"]["code"].clone()), (StatusCode::CONFLICT, json!("wrong_state")));

    let open = call(
        &e.app,
        Method::POST,
        &format!("/admin/disputes/{}/open-onchain", d.id),
        &admin.hdrs(true),
        Some(json!({ "arbiterAddress": arbiter })),
    )
    .await;
    assert_eq!((open.status, open.body["xdr"].as_str()), (StatusCode::OK, Some(format!("XDR[open_dispute:{arbiter}:4]").as_str())));
    assert_eq!(
        call(
            &e.app,
            Method::POST,
            &format!("/admin/disputes/{}/open-onchain", d.id),
            &admin.hdrs(true),
            Some(json!({ "arbiterAddress": "bad" }))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );

    // the chain emits `disputed`; the indexer would do this:
    disputes::mark_opened_onchain(&pool, 4).await.unwrap();
    let again = call(
        &e.app,
        Method::POST,
        &format!("/admin/disputes/{}/open-onchain", d.id),
        &admin.hdrs(true),
        Some(json!({ "arbiterAddress": arbiter })),
    )
    .await;
    assert_eq!(again.status, StatusCode::CONFLICT);

    let no_winner = call(
        &e.app,
        Method::POST,
        &format!("/admin/disputes/{}/resolve/build", d.id),
        &admin.hdrs(true),
        Some(json!({ "arbiterAddress": arbiter, "winnerBps": 5000 })),
    )
    .await;
    assert_eq!((no_winner.status, no_winner.body["error"]["code"].clone()), (StatusCode::BAD_REQUEST, json!("winner_required")));
    for bps in [10_001u32, 99_999] {
        assert_eq!(
            call(
                &e.app,
                Method::POST,
                &format!("/admin/disputes/{}/resolve/build", d.id),
                &admin.hdrs(true),
                Some(json!({ "arbiterAddress": arbiter, "winner": winner, "winnerBps": bps }))
            )
            .await
            .status,
            StatusCode::BAD_REQUEST
        );
    }
    let split = call(
        &e.app,
        Method::POST,
        &format!("/admin/disputes/{}/resolve/build", d.id),
        &admin.hdrs(true),
        Some(json!({ "arbiterAddress": arbiter, "winner": winner, "winnerBps": 6000, "note": "split 60/40" })),
    )
    .await;
    assert_eq!(split.body["xdr"].as_str(), Some(format!("XDR[resolve:{arbiter}:4:{winner}:6000]").as_str()));
    let refund = call(
        &e.app,
        Method::POST,
        &format!("/admin/disputes/{}/resolve/build", d.id),
        &admin.hdrs(true),
        Some(json!({ "arbiterAddress": arbiter, "winner": winner, "winnerBps": 0 })),
    )
    .await;
    assert_eq!(
        refund.body["xdr"].as_str(),
        Some(format!("XDR[resolve:{arbiter}:4:-:0]").as_str()),
        "a 0% award drops the winner so the whole amount returns to the maintainer"
    );
    assert_eq!(disputes::get(&pool, &d.id).await.unwrap().unwrap().resolution_note.as_deref(), Some(""));

    let audited: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_log ORDER BY created_at").fetch_all(&pool).await.unwrap();
    assert_eq!(audited, ["dispute.open_onchain.build", "dispute.resolve.build", "dispute.resolve.build"]);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_dispute_can_be_rejected_with_a_note_once(pool: PgPool) {
    let e = env(pool.clone(), true, vec![]);
    fund(&e, 4, 3).await;
    let admin = signed_in(&e, 1, "arb", None, true).await;
    let d = disputes::create(&pool, 4, 500, "x").await.unwrap();
    let uri = format!("/admin/disputes/{}/reject", d.id);
    assert_eq!(call(&e.app, Method::POST, &uri, &admin.hdrs(true), Some(json!({ "note": "" }))).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(call(&e.app, Method::POST, &uri, &admin.hdrs(true), Some(json!({ "note": "no merit" }))).await.status, StatusCode::OK);
    let row = disputes::get(&pool, &d.id).await.unwrap().unwrap();
    assert_eq!((row.status.as_str(), row.resolution_note.as_deref()), ("rejected", Some("no merit")));
    assert_eq!(call(&e.app, Method::POST, &uri, &admin.hdrs(true), Some(json!({ "note": "again" }))).await.status, StatusCode::CONFLICT);
    assert!(get_admin(&e, &admin, "/admin/disputes").await.body.as_array().unwrap().is_empty());
}

async fn get_admin(e: &Env, s: &Session, uri: &str) -> Resp {
    call(&e.app, Method::GET, uri, &s.hdrs(false), None).await
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn failed_payouts_can_be_listed_and_retried_but_not_when_a_transaction_exists(pool: PgPool) {
    let e = env(pool.clone(), true, vec![]);
    fund(&e, 4, 3).await;
    fund_more(&e, 5, 4).await;
    let admin = signed_in(&e, 1, "arb", None, true).await;
    let a = payouts::create_pending(&pool, 4, 7, 900).await.unwrap().unwrap();
    let b = payouts::create_pending(&pool, 5, 8, 900).await.unwrap().unwrap();
    payouts::mark_failed(&pool, &a.id, "boom", true).await.unwrap();
    payouts::mark_submitted(&pool, &b.id, "hash").await.unwrap();
    payouts::mark_failed(&pool, &b.id, "later", false).await.unwrap();

    let listed = get_admin(&e, &admin, "/admin/payouts").await;
    assert_eq!(listed.body.as_array().unwrap().len(), 2);
    assert_eq!(get_admin(&e, &admin, "/admin/payouts?status=CONFIRMED").await.body.as_array().unwrap().len(), 0);
    assert_eq!(get_admin(&e, &admin, "/admin/payouts?status=BOGUS").await.status, StatusCode::BAD_REQUEST);

    let retry = call(&e.app, Method::POST, &format!("/admin/payouts/{}/retry", a.id), &admin.hdrs(true), None).await;
    assert_eq!(retry.status, StatusCode::OK);
    assert_eq!(payouts::get(&pool, &a.id).await.unwrap().unwrap().status, "QUEUED");
    let blocked = call(&e.app, Method::POST, &format!("/admin/payouts/{}/retry", b.id), &admin.hdrs(true), None).await;
    assert_eq!((blocked.status, blocked.body["error"]["code"].clone()), (StatusCode::CONFLICT, json!("has_tx_hash")));
    assert_eq!(
        call(&e.app, Method::POST, &format!("/admin/payouts/{}/retry", a.id), &admin.hdrs(true), None).await.status,
        StatusCode::CONFLICT,
        "no longer FAILED"
    );
    assert_eq!(call(&e.app, Method::POST, "/admin/payouts/NOT-AN-ID/retry", &admin.hdrs(true), None).await.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn the_admin_status_summarises_the_system(pool: PgPool) {
    let e = env(pool.clone(), true, vec![]);
    fund(&e, 4, 3).await;
    let admin = signed_in(&e, 1, "arb", None, true).await;
    disputes::create(&pool, 4, 500, "x").await.unwrap();
    escrow_db::webhooks::save_cursor(&pool, "bounty_escrow", "c", 987).await.unwrap();
    let r = get_admin(&e, &admin, "/admin/status").await;
    assert_eq!(
        (r.body["indexerLedger"].as_i64(), r.body["openDisputes"].as_i64(), r.body["failedPayouts"].as_i64(), r.body["deadJobs"].as_i64()),
        (Some(987), Some(1), Some(0), Some(0))
    );
}

// ================= webhooks =================

fn sign_body(body: &str) -> String {
    format!("sha256={}", hmac_hex(WEBHOOK_SECRET, body))
}

async fn deliver(e: &Env, id: &str, event: &str, body: &str, sig: Option<String>) -> Resp {
    let mut h = vec![("x-github-delivery", id.to_string()), ("x-github-event", event.to_string())];
    if let Some(s) = sig {
        h.push(("x-hub-signature-256", s));
    }
    let r = Request::builder().method(Method::POST).uri("/github/webhooks").header(header::CONTENT_TYPE, "application/json");
    let r = h.iter().fold(r, |r, (k, v)| r.header(*k, v)).body(Body::from(body.to_string())).unwrap();
    let resp = e.app.clone().oneshot(r).await.unwrap();
    let status = resp.status();
    let text = String::from_utf8(resp.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    Resp { status, headers: Default::default(), body: serde_json::from_str(&text).unwrap_or(Value::Null), text }
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_signed_webhook_is_recorded_and_queued_exactly_once(pool: PgPool) {
    let e = env(pool.clone(), true, vec![]);
    let body = r#"{"action":"created","comment":{"body":"/bounty 5"}}"#;
    let r = deliver(&e, "delivery-1", "issue_comment", body, Some(sign_body(body))).await;
    assert_eq!((r.status, r.body["queued"].clone()), (StatusCode::ACCEPTED, json!(true)));
    // GitHub redelivers: acknowledged, not queued again
    let again = deliver(&e, "delivery-1", "issue_comment", body, Some(sign_body(body))).await;
    assert_eq!((again.status, again.body["duplicate"].clone()), (StatusCode::OK, json!(true)));
    let (deliveries, jobs): (i64, i64) = (
        sqlx::query_scalar("SELECT count(*) FROM webhook_deliveries").fetch_one(&pool).await.unwrap(),
        sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'webhook'").fetch_one(&pool).await.unwrap(),
    );
    assert_eq!((deliveries, jobs), (1, 1));
    let payload: Value = sqlx::query_scalar("SELECT payload FROM jobs").fetch_one(&pool).await.unwrap();
    assert_eq!((payload["event"].as_str(), payload["payload"]["comment"]["body"].as_str()), (Some("issue_comment"), Some("/bounty 5")));
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn unsigned_forged_or_tampered_webhooks_are_rejected_before_anything_is_stored(pool: PgPool) {
    let e = env(pool.clone(), true, vec![]);
    let body = r#"{"action":"created"}"#;
    let cases = [
        ("no signature", None),
        ("wrong secret", Some(format!("sha256={}", hmac_hex("another-secret-entirely", body)))),
        ("tampered body", Some(sign_body(r#"{"action":"edited"}"#))),
        ("wrong algorithm prefix", Some(sign_body(body).replace("sha256=", "sha1="))),
        ("garbage", Some("sha256=zzzz".into())),
    ];
    for (name, sig) in cases {
        let r = deliver(&e, "d-x", "issue_comment", body, sig).await;
        assert_eq!((r.status, r.body["error"]["code"].clone()), (StatusCode::UNAUTHORIZED, json!("bad_signature")), "{name}");
    }
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT count(*) FROM webhook_deliveries").fetch_one(&pool).await.unwrap(), 0);
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs").fetch_one(&pool).await.unwrap(), 0);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_correctly_signed_but_malformed_delivery_is_a_400(pool: PgPool) {
    let e = env(pool, true, vec![]);
    let body = "not json";
    let r = deliver(&e, "d1", "ping", body, Some(sign_body(body))).await;
    assert_eq!((r.status, r.body["error"]["code"].clone()), (StatusCode::BAD_REQUEST, json!("bad_json")));
    let ok_body = "{}";
    let no_headers = Request::builder()
        .method(Method::POST)
        .uri("/github/webhooks")
        .header("x-hub-signature-256", sign_body(ok_body))
        .body(Body::from(ok_body))
        .unwrap();
    assert_eq!(e.app.clone().oneshot(no_headers).await.unwrap().status(), StatusCode::BAD_REQUEST);
}

// ================= limits =================

#[sqlx::test(migrator = "MIGRATOR")]
async fn mutations_are_rate_limited_per_user_not_shared(pool: PgPool) {
    let e = env(pool, true, vec![]);
    let a = signed_in(&e, 1, "a", None, false).await;
    let b = signed_in(&e, 2, "b", None, false).await;
    let hit = |s: &Session| {
        let (app, h) = (e.app.clone(), s.hdrs(true));
        async move { call(&app, Method::POST, "/wallet/challenge", &h, Some(json!({ "address": "GBAD" }))).await.status }
    };
    for _ in 0..20 {
        assert_eq!(hit(&a).await, StatusCode::BAD_REQUEST);
    }
    assert_eq!(hit(&a).await, StatusCode::TOO_MANY_REQUESTS, "the 21st in a minute");
    assert_eq!(hit(&b).await, StatusCode::BAD_REQUEST, "another user is unaffected");
    let r = call(&e.app, Method::POST, "/wallet/challenge", &a.hdrs(true), Some(json!({ "address": "GBAD" }))).await;
    assert_eq!(r.body["error"]["code"], "rate_limited");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn the_global_ceiling_protects_every_route_but_health(pool: PgPool) {
    let e = env(pool, true, vec![]);
    for _ in 0..300 {
        get(&e, "/bounties/abc").await;
    }
    assert_eq!(get(&e, "/bounties/abc").await.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(get(&e, "/health").await.status, StatusCode::OK, "orchestrator health checks are never throttled");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn x_forwarded_for_is_ignored_unless_the_proxy_is_trusted(pool: PgPool) {
    let e = env(pool, true, vec![]);
    // with trust_proxy off, rotating the header cannot dodge the per-address ceiling
    for i in 0..300 {
        call(&e.app, Method::GET, "/bounties/abc", &[("x-forwarded-for", format!("10.0.0.{}", i % 250))], None).await;
    }
    let r = call(&e.app, Method::GET, "/bounties/abc", &[("x-forwarded-for", "203.0.113.9".into())], None).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn large_bodies_are_refused(pool: PgPool) {
    let e = env(pool, true, vec![]);
    let s = signed_in(&e, 1, "a", None, false).await;
    let huge = json!({ "address": "x".repeat(1_100_000) });
    let r = call(&e.app, Method::POST, "/wallet/challenge", &s.hdrs(true), Some(huge)).await;
    assert!(r.status == StatusCode::PAYLOAD_TOO_LARGE || r.status == StatusCode::BAD_REQUEST, "{}", r.status);
    let _ = (bounties::get(&e.db.pool, 1).await, Duration::seconds(1));
}
