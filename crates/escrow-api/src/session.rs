//! Sessions and CSRF. The cookie holds a random id; the database holds only its SHA-256.

use crate::{
    error::{ApiError, ApiResult},
    state::AppState,
};
use axum::http::HeaderMap;
use chrono::{DateTime, Duration, Utc};
use escrow_core::{
    oauth::{SESSION_COOKIE, cookie_value},
    security::{csrf_valid, hash_session_id, is_session_id, random_hex, sha256_hex},
};
use escrow_db::{Db, User, sessions};

pub const SESSION_DAYS: i64 = 30;

pub async fn create(db: &Db, user_id: &str, now: DateTime<Utc>) -> Result<(String, i64), sqlx::Error> {
    let id = random_hex::<32>();
    sessions::create(&db.pool, &hash_session_id(&id), user_id, now + Duration::days(SESSION_DAYS)).await?;
    Ok((id, SESSION_DAYS * 86_400))
}

pub fn cookie_id(h: &HeaderMap) -> Option<String> {
    let raw = h.get(axum::http::header::COOKIE)?.to_str().ok()?;
    cookie_value(raw, SESSION_COOKIE).filter(|v| is_session_id(v)).map(str::to_string)
}

pub struct Authed {
    pub user: User,
    pub session_id: String,
}

/// Rate-limit key: the signed-in session (per user), otherwise the client address.
pub fn limit_key(h: &HeaderMap, ip: &str) -> String {
    cookie_id(h).map_or_else(|| format!("ip:{ip}"), |id| format!("u:{}", &sha256_hex(&id)[..16]))
}

/// The signed-in user, or 401. Mutating requests must also carry the CSRF token bound to the session.
pub async fn require_user(s: &AppState, h: &HeaderMap, mutating: bool) -> ApiResult<Authed> {
    let sid = cookie_id(h).ok_or_else(ApiError::unauthenticated)?;
    let user = sessions::user_for(&s.db.pool, &hash_session_id(&sid), (s.now)()).await?.ok_or_else(ApiError::unauthenticated)?;
    if mutating {
        let given = h.get("x-csrf-token").and_then(|v| v.to_str().ok());
        if !csrf_valid(&s.cfg.csrf_secret, &sid, given) {
            return Err(ApiError::forbidden("csrf", "missing or invalid CSRF token"));
        }
    }
    Ok(Authed { user, session_id: sid })
}

pub async fn destroy(db: &Db, session_id: &str) -> Result<(), sqlx::Error> {
    sessions::delete(&db.pool, &hash_session_id(session_id)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(cookie: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("cookie", HeaderValue::from_str(cookie).unwrap());
        h
    }

    #[test]
    fn only_well_formed_session_cookies_are_read() {
        let id = random_hex::<32>();
        assert_eq!(cookie_id(&headers(&format!("a=1; se_session={id}; b=2"))), Some(id));
        for bad in ["se_session=short", "se_session=", "other=abc", &format!("se_session={}", "Z".repeat(64))] {
            assert_eq!(cookie_id(&headers(bad)), None, "{bad}");
        }
        assert_eq!(cookie_id(&HeaderMap::new()), None);
    }

    #[test]
    fn the_limit_key_prefers_the_session_and_never_exposes_it() {
        let id = random_hex::<32>();
        let k = limit_key(&headers(&format!("se_session={id}")), "1.2.3.4");
        assert!(k.starts_with("u:") && !k.contains(&id) && k.len() == 18);
        assert_eq!(limit_key(&HeaderMap::new(), "1.2.3.4"), "ip:1.2.3.4");
    }
}
