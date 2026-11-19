//! GitHub sign-in helpers: PKCE, the signed state cookie that binds the flow to one browser, and
//! a redirect allow-list. The network calls live in `escrow-github`; this part is pure.

use crate::security::{ct_eq, hmac_b64url, random_b64url};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const OAUTH_COOKIE: &str = "se_oauth";
pub const SESSION_COOKIE: &str = "se_session";
pub const STATE_TTL_MS: i64 = 10 * 60 * 1000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OauthState {
    pub state: String,
    pub verifier: String,
    /// expiry, unix milliseconds
    pub exp: i64,
    pub next: String,
}

/// Only same-site relative paths: never an absolute or protocol-relative URL (open redirect).
pub fn safe_next(next: Option<&str>) -> String {
    let ok = |n: &str| {
        n.len() <= 200
            && n.starts_with('/')
            && !n[1..].starts_with(['/', '\\'])
            && n.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./?=&%~".contains(&b))
    };
    match next {
        Some(n) if ok(n) => n.to_string(),
        _ => "/me".to_string(),
    }
}

pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

pub struct StartedLogin {
    pub authorize_url: String,
    /// value of the `se_oauth` cookie: `payload.mac`
    pub cookie_value: String,
}

/// Step 1: the GitHub authorize URL plus the signed cookie that binds the flow to this browser.
pub fn start_login(client_id: &str, callback_url: &str, secret: &str, next: Option<&str>, now_ms: i64) -> StartedLogin {
    let st =
        OauthState { state: random_b64url::<24>(), verifier: random_b64url::<48>(), exp: now_ms + STATE_TTL_MS, next: safe_next(next) };
    let challenge = pkce_challenge(&st.verifier);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&st).expect("state serializes"));
    let mac = hmac_b64url(secret, &payload);
    let q = form_urlencode(&[
        ("client_id", client_id),
        ("redirect_uri", callback_url),
        ("state", &st.state),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
    ]);
    StartedLogin { authorize_url: format!("https://github.com/login/oauth/authorize?{q}"), cookie_value: format!("{payload}.{mac}") }
}

#[derive(Debug, PartialEq, Eq)]
pub enum StateError {
    Invalid,
    Expired,
    Mismatch,
}

/// Step 2 (first half): verify the cookie's MAC, its expiry and that `state` matches the query.
pub fn verify_state(secret: &str, cookie: &str, query_state: &str, now_ms: i64) -> Result<OauthState, StateError> {
    let (payload, mac) = cookie.split_once('.').ok_or(StateError::Invalid)?;
    if !ct_eq(mac, &hmac_b64url(secret, payload)) {
        return Err(StateError::Invalid);
    }
    let bytes = URL_SAFE_NO_PAD.decode(payload).map_err(|_| StateError::Invalid)?;
    let st: OauthState = serde_json::from_slice(&bytes).map_err(|_| StateError::Invalid)?;
    if st.exp <= now_ms {
        return Err(StateError::Expired);
    }
    if !ct_eq(&st.state, query_state) {
        return Err(StateError::Mismatch);
    }
    Ok(st)
}

pub fn form_urlencode(pairs: &[(&str, &str)]) -> String {
    pairs.iter().map(|(k, v)| format!("{}={}", pct(k), pct(v))).collect::<Vec<_>>().join("&")
}

fn pct(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn session_cookie(id: &str, max_age_secs: i64, secure: bool) -> String {
    format!("{SESSION_COOKIE}={id}; Path=/; Max-Age={max_age_secs}; HttpOnly; SameSite=Lax{}", if secure { "; Secure" } else { "" })
}

pub fn state_cookie(value: &str, secure: bool) -> String {
    format!(
        "{OAUTH_COOKIE}={value}; Path=/; Max-Age={}; HttpOnly; SameSite=Lax{}",
        STATE_TTL_MS / 1000,
        if secure { "; Secure" } else { "" }
    )
}

pub fn clear_cookie(name: &str) -> String {
    format!("{name}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax")
}

/// Reads one cookie value from a `Cookie` header.
pub fn cookie_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header.split(';').filter_map(|c| c.trim().split_once('=')).find(|(k, _)| *k == name).map(|(_, v)| v)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn next_allows_only_relative_paths() {
        assert_eq!(safe_next(Some("/b/4")), "/b/4");
        assert_eq!(safe_next(Some("/me?tab=wallet")), "/me?tab=wallet");
        for bad in ["https://evil.example", "//evil.example", "/\\evil", "javascript:alert(1)", "b/4", "/a b", "/<script>"] {
            assert_eq!(safe_next(Some(bad)), "/me", "{bad}");
        }
        assert_eq!(safe_next(None), "/me");
        assert_eq!(safe_next(Some(&format!("/{}", "a".repeat(250)))), "/me");
    }

    #[test]
    fn pkce_matches_the_rfc7636_vector() {
        assert_eq!(pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn full_round_trip() {
        let s = start_login("cid", "http://localhost:3001/auth/callback", SECRET, Some("/b/4"), 1_000);
        assert!(s.authorize_url.starts_with("https://github.com/login/oauth/authorize?client_id=cid"));
        assert!(s.authorize_url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A3001%2Fauth%2Fcallback"));
        assert!(s.authorize_url.contains("code_challenge_method=S256"));
        let (payload, _) = s.cookie_value.split_once('.').unwrap();
        let st: OauthState = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap();
        assert!(s.authorize_url.contains(&format!("state={}", st.state)));
        assert!(s.authorize_url.contains(&format!("code_challenge={}", pkce_challenge(&st.verifier))));
        let ok = verify_state(SECRET, &s.cookie_value, &st.state, 2_000).unwrap();
        assert_eq!(ok.next, "/b/4");
    }

    #[test]
    fn tampering_expiry_and_mismatch_are_rejected() {
        let s = start_login("cid", "http://x/cb", SECRET, None, 0);
        let st = {
            let (p, _) = s.cookie_value.split_once('.').unwrap();
            serde_json::from_slice::<OauthState>(&URL_SAFE_NO_PAD.decode(p).unwrap()).unwrap()
        };
        assert_eq!(verify_state("another-secret-another-secret-xx", &s.cookie_value, &st.state, 1), Err(StateError::Invalid));
        assert_eq!(verify_state(SECRET, &s.cookie_value, "wrong", 1), Err(StateError::Mismatch));
        assert_eq!(verify_state(SECRET, &s.cookie_value, &st.state, STATE_TTL_MS + 1), Err(StateError::Expired));
        assert_eq!(verify_state(SECRET, "no-dot", &st.state, 1), Err(StateError::Invalid));
        let (p, m) = s.cookie_value.split_once('.').unwrap();
        let forged = format!("{}.{m}", URL_SAFE_NO_PAD.encode(br#"{"state":"a","verifier":"b","exp":99999999999999,"next":"//evil"}"#));
        assert_eq!(verify_state(SECRET, &forged, "a", 1), Err(StateError::Invalid));
        let _ = p;
    }

    #[test]
    fn cookie_helpers() {
        assert_eq!(cookie_value("a=1; se_session=abc; b=2", "se_session"), Some("abc"));
        assert_eq!(cookie_value("a=1", "se_session"), None);
        assert!(session_cookie("id", 60, true).ends_with("; Secure"));
        assert!(!session_cookie("id", 60, false).contains("Secure"));
        assert!(session_cookie("id", 60, false).contains("HttpOnly"));
        assert!(clear_cookie("x").contains("Max-Age=0"));
    }
}
