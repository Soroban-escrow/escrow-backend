//! Linking a Stellar address to a GitHub account, proven by a wallet signature (SEP-53 `signMessage`).
//! A payout goes to the address linked at merge time, so the proof must be strong: the message binds
//! the GitHub account, the address, a single-use nonce and an expiry.

use crate::error::{ApiError, ApiResult};
use chrono::{DateTime, Duration, Utc};
use escrow_chain::keys;
use escrow_core::security::{random_hex, wallet_challenge_message};
use escrow_db::{Db, is_unique_violation, new_id, sessions, users};
use serde::Serialize;

const TTL: Duration = Duration::minutes(5);

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Challenge {
    pub challenge_id: String,
    pub message: String,
}

pub async fn create_challenge(db: &Db, user_id: &str, address: &str, now: DateTime<Utc>) -> ApiResult<Challenge> {
    if !keys::is_valid_account(address) {
        return Err(ApiError::new(400, "bad_address", "invalid Stellar address"));
    }
    let user = users::get(&db.pool, user_id).await?.ok_or_else(ApiError::unauthenticated)?;
    if users::by_wallet(&db.pool, address).await?.is_some_and(|o| o.id != user_id) {
        return Err(ApiError::conflict("address_taken", "address already linked to another account"));
    }
    let (nonce, expires_at, id) = (random_hex::<32>(), now + TTL, new_id());
    let message = wallet_challenge_message(
        &user.github_login,
        user.github_id,
        address,
        &nonce,
        &expires_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    );
    sessions::create_challenge(&db.pool, &id, user_id, address, &message, &nonce, expires_at).await?;
    Ok(Challenge { challenge_id: id, message })
}

/// Verifies the signature, burns the challenge (single use) and links the wallet. Returns the address.
pub async fn verify_challenge(db: &Db, user_id: &str, challenge_id: &str, signature_b64: &str, now: DateTime<Utc>) -> ApiResult<String> {
    let c = sessions::get_challenge(&db.pool, challenge_id)
        .await?
        .filter(|c| c.user_id == user_id)
        .ok_or_else(|| ApiError::new(404, "challenge_not_found", "challenge not found"))?;
    if c.used_at.is_some() {
        return Err(ApiError::conflict("challenge_used", "challenge already used"));
    }
    if c.expires_at <= now {
        return Err(ApiError::new(410, "challenge_expired", "challenge expired"));
    }
    if !keys::verify_sep53(&c.address, &c.message, signature_b64) {
        return Err(ApiError::new(400, "bad_signature", "signature does not match"));
    }
    let mut tx = db.pool.begin().await?;
    // atomic burn: of several simultaneous verifies exactly one wins
    if !sessions::burn_challenge(&mut *tx, &c.id, now).await? {
        return Err(ApiError::conflict("challenge_used", "challenge already used"));
    }
    match users::set_wallet(&mut *tx, user_id, &c.address, now).await {
        Ok(()) => {}
        Err(e) if is_unique_violation(&e) => return Err(ApiError::conflict("address_taken", "address already linked to another account")),
        Err(e) => return Err(e.into()),
    }
    tx.commit().await?;
    Ok(c.address)
}

pub async fn unlink(db: &Db, user_id: &str) -> Result<(), sqlx::Error> {
    users::clear_wallet(&db.pool, user_id).await
}
