//! Bounties. Status columns are written ONLY by the `apply_*` functions, which the indexer calls
//! from contract events; no HTTP handler can change a bounty's status.

use crate::models::{BOUNTY_COLS, Bounty, Repo};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{FromRow, PgConnection, PgExecutor, Postgres, QueryBuilder};

pub struct NewBounty<'a> {
    pub id: i64,
    pub repo_id: i64,
    pub issue_number: i32,
    pub issue_url: &'a str,
    pub maintainer: &'a str,
    pub maintainer_github_id: Option<i64>,
    pub token: &'a str,
    pub amount: i128,
    pub fee_bps: i32,
    pub deadline: DateTime<Utc>,
    pub relayer_allowed: bool,
    pub fund_tx: &'a str,
    pub created_at: DateTime<Utc>,
    pub last_event_id: &'a str,
}

pub async fn insert<'e>(ex: impl PgExecutor<'e>, b: &NewBounty<'_>) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO bounties (id, repo_id, issue_number, issue_url, maintainer, maintainer_github_id, token, amount, fee_bps, deadline,
                               relayer_allowed, status, fund_tx, created_at, last_event_id)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8::numeric,$9,$10,$11,'OPEN',$12,$13,$14)",
    )
    .bind(b.id)
    .bind(b.repo_id)
    .bind(b.issue_number)
    .bind(b.issue_url)
    .bind(b.maintainer)
    .bind(b.maintainer_github_id)
    .bind(b.token)
    .bind(b.amount.to_string())
    .bind(b.fee_bps)
    .bind(b.deadline)
    .bind(b.relayer_allowed)
    .bind(b.fund_tx)
    .bind(b.created_at)
    .bind(b.last_event_id)
    .execute(ex)
    .await
    .map(|_| ())
}

pub async fn get<'e>(ex: impl PgExecutor<'e>, id: i64) -> Result<Option<Bounty>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {BOUNTY_COLS} FROM bounties WHERE id = $1"))).bind(id).fetch_optional(ex).await
}

/// Row-locks the bounty for the rest of the transaction (the indexer applies events one at a time per bounty).
pub async fn get_for_update(conn: &mut PgConnection, id: i64) -> Result<Option<Bounty>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {BOUNTY_COLS} FROM bounties WHERE id = $1 FOR UPDATE")))
        .bind(id)
        .fetch_optional(conn)
        .await
}

pub async fn with_repo<'e>(ex: impl PgExecutor<'e> + Copy, id: i64) -> Result<Option<(Bounty, Repo)>, sqlx::Error> {
    let Some(b) = get(ex, id).await? else {
        return Ok(None);
    };
    let repo = crate::github::get_repo(ex, b.repo_id).await?.ok_or(sqlx::Error::RowNotFound)?;
    Ok(Some((b, repo)))
}

/// The bounty a new `/bounty` would collide with (Open or Disputed funds are still locked).
pub async fn find_active_for_issue<'e>(ex: impl PgExecutor<'e>, repo_id: i64, issue_number: i32) -> Result<Option<Bounty>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {BOUNTY_COLS} FROM bounties WHERE repo_id = $1 AND issue_number = $2 AND status IN ('OPEN','DISPUTED') ORDER BY id DESC LIMIT 1")))
        .bind(repo_id)
        .bind(issue_number)
        .fetch_optional(ex)
        .await
}

pub async fn latest_for_issue<'e>(ex: impl PgExecutor<'e>, repo_id: i64, issue_number: i32) -> Result<Option<Bounty>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {BOUNTY_COLS} FROM bounties WHERE repo_id = $1 AND issue_number = $2 ORDER BY id DESC LIMIT 1"
    )))
    .bind(repo_id)
    .bind(issue_number)
    .fetch_optional(ex)
    .await
}

pub async fn active_for_issues<'e>(ex: impl PgExecutor<'e>, repo_id: i64, issues: &[i32]) -> Result<Vec<Bounty>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {BOUNTY_COLS} FROM bounties WHERE repo_id = $1 AND issue_number = ANY($2) AND status IN ('OPEN','DISPUTED') ORDER BY id"
    )))
    .bind(repo_id)
    .bind(issues)
    .fetch_all(ex)
    .await
}

pub async fn by_maintainer<'e>(ex: impl PgExecutor<'e>, wallet: &str) -> Result<Vec<Bounty>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {BOUNTY_COLS} FROM bounties WHERE maintainer = $1 ORDER BY id DESC")))
        .bind(wallet)
        .fetch_all(ex)
        .await
}

pub async fn open_ones<'e>(ex: impl PgExecutor<'e>) -> Result<Vec<Bounty>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {BOUNTY_COLS} FROM bounties WHERE status = 'OPEN' ORDER BY id"))).fetch_all(ex).await
}

pub async fn set_issue_data<'e>(ex: impl PgExecutor<'e>, id: i64, title: &str, criteria: &[String]) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE bounties SET issue_title = $2, criteria = $3, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(title)
        .bind(criteria)
        .execute(ex)
        .await
        .map(|_| ())
}

/// When someone edits the issue: refresh title and criteria on its live bounty. Returns rows changed.
pub async fn set_issue_data_for_issue<'e>(
    ex: impl PgExecutor<'e>,
    repo_id: i64,
    issue_number: i32,
    title: &str,
    criteria: &[String],
) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "UPDATE bounties SET issue_title = $3, criteria = $4, updated_at = now()
         WHERE repo_id = $1 AND issue_number = $2 AND status IN ('OPEN','DISPUTED')",
    )
    .bind(repo_id)
    .bind(issue_number)
    .bind(title)
    .bind(criteria)
    .execute(ex)
    .await
    .map(|r| r.rows_affected())
}

pub async fn set_card_comment<'e>(ex: impl PgExecutor<'e>, id: i64, comment_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE bounties SET card_comment_id = $2 WHERE id = $1").bind(id).bind(comment_id).execute(ex).await.map(|_| ())
}

pub async fn card_comment<'e>(ex: impl PgExecutor<'e>, id: i64) -> Result<Option<i64>, sqlx::Error> {
    Ok(sqlx::query_scalar::<_, Option<i64>>("SELECT card_comment_id FROM bounties WHERE id = $1")
        .bind(id)
        .fetch_optional(ex)
        .await?
        .flatten())
}

/// Serialises card refreshes for one bounty for the rest of the transaction. Without it two concurrent
/// refreshes would both see "no card yet" and both post one.
pub async fn lock_card(conn: &mut PgConnection, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(id).execute(conn).await.map(|_| ())
}

// ---- state transitions, applied by the indexer only ----

pub async fn apply_released<'e>(
    ex: impl PgExecutor<'e>,
    id: i64,
    event_id: &str,
    tx: &str,
    winner: &str,
    winner_github_id: Option<i64>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE bounties SET status = 'RELEASED', winner_address = $4, winner_github_id = $5, close_tx = $3, last_event_id = $2, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(event_id)
    .bind(tx)
    .bind(winner)
    .bind(winner_github_id)
    .execute(ex)
    .await
    .map(|_| ())
}

pub async fn apply_refunded<'e>(ex: impl PgExecutor<'e>, id: i64, event_id: &str, tx: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE bounties SET status = 'REFUNDED', close_tx = $3, last_event_id = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(event_id)
        .bind(tx)
        .execute(ex)
        .await
        .map(|_| ())
}

pub async fn apply_disputed<'e>(ex: impl PgExecutor<'e>, id: i64, event_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE bounties SET status = 'DISPUTED', last_event_id = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(event_id)
        .execute(ex)
        .await
        .map(|_| ())
}

pub async fn apply_resolved<'e>(
    ex: impl PgExecutor<'e>,
    id: i64,
    event_id: &str,
    tx: &str,
    winner: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE bounties SET status = 'RESOLVED', winner_address = $4, close_tx = $3, last_event_id = $2, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(event_id)
    .bind(tx)
    .bind(winner)
    .execute(ex)
    .await
    .map(|_| ())
}

// ---- listing ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Newest,
    Highest,
    Ending,
}

#[derive(Debug, Clone)]
pub struct ListFilter {
    pub status: String,
    pub token: Option<String>,
    pub min_amount: Option<String>,
    pub q: Option<String>,
    pub sort: Sort,
    pub cursor: Option<i64>,
    pub limit: i64,
}

impl Default for ListFilter {
    fn default() -> Self {
        Self { status: "OPEN".into(), token: None, min_amount: None, q: None, sort: Sort::Newest, cursor: None, limit: 20 }
    }
}

#[derive(Debug, Serialize)]
pub struct RepoName {
    #[serde(rename = "fullName")]
    pub full_name: String,
}

#[derive(Debug, Serialize)]
pub struct PrCount {
    pub prs: i64,
}

/// A bounty with its repository name and number of linked PRs, as the list endpoint returns it.
#[derive(Debug, Serialize)]
pub struct Listed {
    #[serde(flatten)]
    pub bounty: Bounty,
    pub repo: RepoName,
    #[serde(rename = "_count")]
    pub count: PrCount,
}

#[derive(FromRow)]
struct ListRow {
    #[sqlx(flatten)]
    bounty: Bounty,
    repo_full_name: String,
    pr_count: i64,
}

/// `BOUNTY_COLS` with every column prefixed by the `b` alias.
pub(crate) fn qualified_cols() -> String {
    BOUNTY_COLS
        .split(", ")
        .map(|c| if let Some(rest) = c.strip_prefix("amount::text") { format!("b.amount::text{rest}") } else { format!("b.{c}") })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Escapes `%`, `_` and `\` so user text matches literally in ILIKE.
pub fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// Returns up to `limit + 1` rows so the caller can tell whether another page exists.
pub async fn list<'e>(ex: impl PgExecutor<'e>, f: &ListFilter) -> Result<Vec<Listed>, sqlx::Error> {
    let mut q: QueryBuilder<Postgres> = QueryBuilder::new(format!(
        "SELECT {}, r.full_name AS repo_full_name, (SELECT count(*) FROM pull_request_bounties l WHERE l.bounty_id = b.id) AS pr_count
         FROM bounties b JOIN repos r ON r.id = b.repo_id WHERE b.status = ",
        qualified_cols()
    ));
    q.push_bind(f.status.clone());
    if let Some(t) = &f.token {
        q.push(" AND b.token = ").push_bind(t.clone());
    }
    if let Some(m) = &f.min_amount {
        q.push(" AND b.amount >= ").push_bind(m.clone()).push("::numeric");
    }
    if let Some(text) = &f.q {
        let pat = format!("%{}%", like_escape(text));
        q.push(" AND (b.issue_title ILIKE ").push_bind(pat.clone()).push(" OR r.full_name ILIKE ").push_bind(pat).push(")");
    }
    if let Some(c) = f.cursor {
        // continue strictly after the cursor row in the chosen order
        match f.sort {
            Sort::Newest => {
                q.push(" AND b.id < ").push_bind(c);
            }
            Sort::Highest => {
                q.push(" AND (b.amount, b.id) < (SELECT amount, id FROM bounties WHERE id = ").push_bind(c).push(")");
            }
            Sort::Ending => {
                q.push(" AND (b.deadline > (SELECT deadline FROM bounties WHERE id = ").push_bind(c);
                q.push(") OR (b.deadline = (SELECT deadline FROM bounties WHERE id = ").push_bind(c);
                q.push(") AND b.id < ").push_bind(c).push("))");
            }
        }
    }
    q.push(match f.sort {
        Sort::Newest => " ORDER BY b.id DESC",
        Sort::Highest => " ORDER BY b.amount DESC, b.id DESC",
        Sort::Ending => " ORDER BY b.deadline ASC, b.id DESC",
    });
    q.push(" LIMIT ").push_bind(f.limit + 1);
    let rows: Vec<ListRow> = q.build_query_as().fetch_all(ex).await?;
    Ok(rows
        .into_iter()
        .map(|r| Listed { bounty: r.bounty, repo: RepoName { full_name: r.repo_full_name }, count: PrCount { prs: r.pr_count } })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_patterns_match_literally() {
        assert_eq!(like_escape("100%_off\\"), "100\\%\\_off\\\\");
        assert_eq!(like_escape("plain"), "plain");
    }

    #[test]
    fn column_qualification_keeps_the_amount_cast() {
        let q = qualified_cols();
        assert!(q.starts_with("b.id, b.repo_id"));
        assert!(q.contains("b.amount::text AS amount"));
        assert!(!q.contains(", id,"));
    }
}
