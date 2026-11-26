pub mod admin;
pub mod auth;
pub mod funding;
pub mod public;
pub mod webhook;

use crate::{
    error::{ApiError, ApiResult},
    state::AppState,
};
use chrono::{DateTime, SecondsFormat, Utc};
use escrow_chain::BuiltTx;
use escrow_core::BountyStatus;
use serde_json::{Value, json};

/// #134 — Require the on-chain bounty status to match `expected` before a
/// transaction is built. Returns 409 with the actual chain status when it
/// does not, so an arbiter sees "bounty is Released" rather than the raw
/// contract error that falls out of a doomed simulation.
///
/// Shared between the maintainer routes (release / refund, expect `Open`)
/// and the admin dispute routes (open-on-chain expects `Open`; resolve/build
/// expects `Disputed`).
pub(crate) async fn require_chain_status(s: &AppState, bounty_id: i64, expected: BountyStatus) -> ApiResult<()> {
    // never trust the database for a money decision
    let chain = s.chain.get_bounty(u64::try_from(bounty_id).unwrap_or(0)).await?;
    if chain.status != expected {
        return Err(ApiError::conflict(
            match expected {
                BountyStatus::Open => "not_open",
                BountyStatus::Disputed => "not_disputed",
                _ => "wrong_chain_state",
            },
            &format!("bounty is {}", chain.status),
        ));
    }
    Ok(())
}

pub(crate) fn prepared_tx(built: BuiltTx, winner: Option<&str>) -> ApiResult<Value> {
    let valid_until = DateTime::<Utc>::from_timestamp(i64::try_from(built.valid_until).unwrap_or(i64::MAX), 0)
        .ok_or_else(ApiError::internal)?
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    let mut body = json!({ "xdr": built.xdr, "validUntil": valid_until });
    if let Some(w) = winner {
        body["winner"] = json!(w);
    }
    Ok(body)
}
