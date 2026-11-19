//! Database fixtures.

use chrono::{DateTime, Duration, Utc};
use escrow_db::{Db, User, bounties, github, users};

pub const REPO_ID: i64 = 10;
pub const REPO: &str = "acme/widgets";
pub const INSTALLATION: i64 = 1;
pub const MAINTAINER_WALLET: &str = "GMAINTAINERWALLET";

pub async fn repo(db: &Db) {
    github::upsert_installation(&db.pool, INSTALLATION, "acme", "Organization", None).await.unwrap();
    github::upsert_repo(&db.pool, REPO_ID, INSTALLATION, REPO, false).await.unwrap();
}

pub struct B {
    pub id: i64,
    pub issue: i32,
    pub amount: i128,
    pub relayer_allowed: bool,
    pub deadline: DateTime<Utc>,
    pub maintainer_github_id: Option<i64>,
}

impl B {
    pub fn new(id: i64, issue: i32) -> Self {
        Self {
            id,
            issue,
            amount: 50_000_000,
            relayer_allowed: true,
            deadline: Utc::now() + Duration::days(14),
            maintainer_github_id: Some(500),
        }
    }
}

pub async fn bounty(db: &Db, b: &B) {
    bounties::insert(
        &db.pool,
        &bounties::NewBounty {
            id: b.id,
            repo_id: REPO_ID,
            issue_number: b.issue,
            issue_url: &format!("https://github.com/{REPO}/issues/{}", b.issue),
            maintainer: MAINTAINER_WALLET,
            maintainer_github_id: b.maintainer_github_id,
            token: "CTOKEN",
            amount: b.amount,
            fee_bps: 300,
            deadline: b.deadline,
            relayer_allowed: b.relayer_allowed,
            fund_tx: "fundtx",
            created_at: Utc::now(),
            last_event_id: &format!("{:019}-0000000001", b.id),
        },
    )
    .await
    .unwrap();
}

pub async fn user(db: &Db, github_id: i64, login: &str, wallet: Option<&str>) -> User {
    let u = users::upsert_github(&db.pool, github_id, login, None, false).await.unwrap();
    if let Some(w) = wallet {
        users::set_wallet(&db.pool, &u.id, w, Utc::now()).await.unwrap();
    }
    users::get(&db.pool, &u.id).await.unwrap().unwrap()
}
