//! Channel accounts: extra Stellar accounts that pay fees and provide sequence numbers for relayer releases.
//!
//! A Stellar account has one sequence number, so two transactions built from the same account collide.
//! With channels the relayer key is no longer the transaction source. It only signs its own `release`
//! authorisation, and each release is built and submitted from a different idle channel, so several can be in
//! flight at once. A channel whose submission outcome is unknown is quarantined: its sequence number cannot be trusted.
//! Leases live in memory, so a restarted process starts with every channel idle; see docs/ARCHITECTURE.md.

use crate::{error::ChainError, keys::Signer};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex, MutexGuard};
use stellar_xdr::{
    AccountId, Hash, HashIdPreimage, HashIdPreimageSorobanAuthorization, Limits, PublicKey, ScAddress, ScBytes, ScMap, ScMapEntry, ScSymbol,
    ScVal, ScVec, SorobanAuthorizationEntry, SorobanCredentials, Uint256, WriteXdr,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    Leased,
    Quarantined,
}

pub struct ChannelPool {
    signers: Vec<Signer>,
    state: Mutex<Vec<State>>,
}

impl ChannelPool {
    pub fn new(signers: Vec<Signer>) -> Result<Self, ChainError> {
        if signers.is_empty() {
            return Err(ChainError::Invalid("at least one channel account is required".into()));
        }
        let n = signers.len();
        Ok(Self { signers, state: Mutex::new(vec![State::Idle; n]) })
    }

    pub fn len(&self) -> usize {
        self.signers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.signers.is_empty()
    }

    /// Channels taken out of rotation after an unknown submission outcome.
    pub fn quarantined(&self) -> usize {
        self.states().iter().filter(|s| **s == State::Quarantined).count()
    }

    /// Leases an idle channel, or `None` when every channel is busy or quarantined.
    pub fn lease(self: &Arc<Self>) -> Option<Lease> {
        let mut st = self.states();
        let idx = st.iter().position(|s| *s == State::Idle)?;
        st[idx] = State::Leased;
        Some(Lease { pool: self.clone(), idx })
    }

    fn states(&self) -> MutexGuard<'_, Vec<State>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A channel held for one submission. Dropping it returns the channel to the pool unless it was quarantined.
pub struct Lease {
    pool: Arc<ChannelPool>,
    idx: usize,
}

impl Lease {
    pub fn signer(&self) -> &Signer {
        &self.pool.signers[self.idx]
    }

    /// The submission outcome is unknown, so keep this channel out of rotation until the process restarts.
    pub fn quarantine(self) {
        self.pool.states()[self.idx] = State::Quarantined;
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut st = self.pool.states();
        if st[self.idx] == State::Leased {
            st[self.idx] = State::Idle;
        }
    }
}

/// Signs the relayer's authorisation for a call, when the entry is the relayer's. The relayer account signs the
/// preimage (network, nonce, expiry and invocation), and the signature goes in the credentials. Other entries are
/// left alone. Returns whether the entry was signed.
pub fn authorize_for_relayer(entry: &mut SorobanAuthorizationEntry, relayer: &Signer, network_id: [u8; 32]) -> Result<bool, ChainError> {
    let SorobanCredentials::Address(creds) = &mut entry.credentials else {
        return Ok(false);
    };
    let ScAddress::Account(AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(who)))) = &creds.address else {
        return Ok(false);
    };
    if *who != relayer.public_bytes() {
        return Ok(false);
    }
    let preimage = HashIdPreimage::SorobanAuthorization(HashIdPreimageSorobanAuthorization {
        network_id: Hash(network_id),
        nonce: creds.nonce,
        signature_expiration_ledger: creds.signature_expiration_ledger,
        invocation: entry.root_invocation.clone(),
    });
    let hash: [u8; 32] = Sha256::digest(preimage.to_xdr(Limits::none())?).into();
    creds.signature = account_signature(relayer.public_bytes(), relayer.sign(&hash))?;
    Ok(true)
}

/// The signature value a Stellar account authorisation expects: a vector holding one map with the keys
/// `public_key` and `signature` (in that order).
fn account_signature(public: [u8; 32], sig: [u8; 64]) -> Result<ScVal, ChainError> {
    let invalid = |_| ChainError::Invalid("authorisation signature too large".into());
    let sym = |k: &str| ScSymbol::try_from(k).map(ScVal::Symbol).map_err(|_| ChainError::Invalid(format!("bad symbol {k}")));
    let map = ScMap(
        vec![
            ScMapEntry { key: sym("public_key")?, val: ScVal::Bytes(ScBytes::try_from(public.to_vec()).map_err(invalid)?) },
            ScMapEntry { key: sym("signature")?, val: ScVal::Bytes(ScBytes::try_from(sig.to_vec()).map_err(invalid)?) },
        ]
        .try_into()
        .map_err(invalid)?,
    );
    Ok(ScVal::Vec(Some(ScVec(vec![ScVal::Map(Some(map))].try_into().map_err(invalid)?))))
}

/// Parses channel secrets, refusing an empty list.
pub fn signers_from_secrets(secrets: &[String]) -> Result<Vec<Signer>, ChainError> {
    secrets.iter().map(|s| Signer::from_secret(s)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::testing::identity;

    fn pool(n: u8) -> Arc<ChannelPool> {
        Arc::new(ChannelPool::new((1..=n).map(|i| identity(i).1).collect()).unwrap())
    }

    #[test]
    fn leases_are_exclusive_and_return_on_drop() {
        let p = pool(2);
        let a = p.lease().expect("first lease");
        let b = p.lease().expect("second lease");
        assert!(p.lease().is_none(), "every channel is busy");
        drop(a);
        assert!(p.lease().is_some(), "a dropped lease frees its channel");
        drop(b);
    }

    #[test]
    fn a_quarantined_channel_stays_out_of_rotation() {
        let p = pool(2);
        p.lease().unwrap().quarantine();
        assert_eq!(p.quarantined(), 1);
        let held = p.lease().expect("the other channel is still idle");
        assert!(p.lease().is_none(), "the quarantined channel is not handed out again");
        drop(held);
    }

    #[test]
    fn an_empty_pool_is_refused() {
        assert!(ChannelPool::new(vec![]).is_err());
    }
}
