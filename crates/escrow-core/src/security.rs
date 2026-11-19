//! Security primitives. Everything here is constant-time where it compares secrets, and nothing logs a secret.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, KeyInit, Mac};
use rand::Rng;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// Verifies GitHub's `X-Hub-Signature-256` over the RAW request body in constant time.
pub fn verify_webhook_signature(secret: &str, raw_body: &[u8], header: Option<&str>) -> bool {
    let Some(hex_sig) = header.and_then(|h| h.strip_prefix("sha256=")) else {
        return false;
    };
    let Ok(given) = hex::decode(hex_sig) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(raw_body);
    mac.verify_slice(&given).is_ok()
}

pub fn sha256_hex(input: &str) -> String {
    hex::encode(Sha256::digest(input.as_bytes()))
}

pub fn hmac_hex(secret: &str, message: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(message.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

pub fn hmac_b64url(secret: &str, message: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(message.as_bytes());
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

/// Constant-time string equality.
pub fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    rand::rng().fill_bytes(&mut buf);
    buf
}

pub fn random_hex<const N: usize>() -> String {
    hex::encode(random_bytes::<N>())
}

pub fn random_b64url<const N: usize>() -> String {
    URL_SAFE_NO_PAD.encode(random_bytes::<N>())
}

/// Session ids are stored hashed: a leaked table is not a set of logins.
pub fn hash_session_id(id: &str) -> String {
    sha256_hex(id)
}

pub fn is_session_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// CSRF token bound to the session: an HMAC of the session id.
pub fn csrf_token(secret: &str, session_id: &str) -> String {
    hmac_hex(secret, session_id)
}

pub fn csrf_valid(secret: &str, session_id: &str, given: Option<&str>) -> bool {
    given.is_some_and(|g| ct_eq(g, &csrf_token(secret, session_id)))
}

const SEP53_PREFIX: &str = "Stellar Signed Message:\n";

/// The 32 bytes a wallet signs for `signMessage` under SEP-53: `sha256(prefix || message)`.
pub fn sep53_hash(message: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(SEP53_PREFIX.as_bytes());
    h.update(message.as_bytes());
    h.finalize().into()
}

/// The text the user signs to link a wallet. Binds the GitHub account, the address, a nonce and an expiry.
pub fn wallet_challenge_message(login: &str, github_id: i64, address: &str, nonce: &str, expires_iso: &str) -> String {
    format!("SkillEscrow wallet link\nGitHub: {login} ({github_id})\nAddress: {address}\nNonce: {nonce}\nExpires: {expires_iso}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn webhook_signature_accepts_only_the_exact_body() {
        let body = br#"{"action":"created"}"#;
        let header = sign("s3cret-value", body);
        assert!(verify_webhook_signature("s3cret-value", body, Some(&header)));
        assert!(!verify_webhook_signature("s3cret-value", b"{\"action\":\"edited\"}", Some(&header)));
        assert!(!verify_webhook_signature("other-secret", body, Some(&header)));
    }

    #[test]
    fn webhook_signature_rejects_malformed_headers() {
        let body = b"x";
        for h in [None, Some(""), Some("sha256="), Some("sha1=abcd"), Some("sha256=zz"), Some("sha256=00")] {
            assert!(!verify_webhook_signature("s3cret-value", body, h), "{h:?}");
        }
    }

    #[test]
    fn known_hmac_vector() {
        // RFC 4231 test case 2
        assert_eq!(hmac_hex("Jefe", "what do ya want for nothing?"), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
    }

    #[test]
    fn csrf_is_bound_to_the_session() {
        let secret = "x".repeat(32);
        let t = csrf_token(&secret, "session-a");
        assert!(csrf_valid(&secret, "session-a", Some(&t)));
        assert!(!csrf_valid(&secret, "session-b", Some(&t)));
        assert!(!csrf_valid(&secret, "session-a", None));
        assert!(!csrf_valid(&secret, "session-a", Some("")));
    }

    #[test]
    fn session_id_shape() {
        assert!(is_session_id(&random_hex::<32>()));
        assert!(!is_session_id("short"));
        assert!(!is_session_id(&"G".repeat(64)));
        assert!(!is_session_id(&"A".repeat(64)));
    }

    #[test]
    fn random_values_differ() {
        assert_ne!(random_hex::<32>(), random_hex::<32>());
        assert_eq!(random_b64url::<24>().len(), 32);
    }

    #[test]
    fn sep53_prefixes_the_message() {
        let expected = Sha256::digest(b"Stellar Signed Message:\nhello");
        assert_eq!(sep53_hash("hello").as_slice(), expected.as_slice());
        assert_ne!(sep53_hash("hello"), sep53_hash("hello!"));
    }

    #[test]
    fn challenge_message_binds_every_field() {
        let m = wallet_challenge_message("octocat", 583231, "GABC", "n0nce", "2026-11-05T12:00:00Z");
        for needle in ["octocat", "583231", "GABC", "n0nce", "2026-11-05T12:00:00Z"] {
            assert!(m.contains(needle));
        }
    }

    #[test]
    fn ct_eq_handles_length_mismatch() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "abcd"));
    }
}
