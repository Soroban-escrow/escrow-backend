//! Stellar keys and addresses. Secret keys live only inside [`Signer`], which never prints them.

use crate::error::ChainError;
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};
use escrow_core::security::sep53_hash;
use stellar_xdr::{AccountId, ContractId, Hash, PublicKey, ScAddress, Uint256};

pub fn account_bytes(g: &str) -> Result<[u8; 32], ChainError> {
    stellar_strkey::ed25519::PublicKey::from_string(g)
        .map(|k| k.0)
        .map_err(|_| ChainError::Invalid(format!("not a Stellar account address: {}", abbreviate(g))))
}

pub fn contract_bytes(c: &str) -> Result<[u8; 32], ChainError> {
    stellar_strkey::Contract::from_string(c).map(|k| k.0).map_err(|_| ChainError::Invalid(format!("not a contract id: {}", abbreviate(c))))
}

pub fn is_valid_account(g: &str) -> bool {
    account_bytes(g).is_ok()
}

pub fn account_strkey(bytes: [u8; 32]) -> String {
    stellar_strkey::ed25519::PublicKey(bytes).to_string().to_string()
}

pub fn contract_strkey(bytes: [u8; 32]) -> String {
    stellar_strkey::Contract(bytes).to_string().to_string()
}

fn abbreviate(s: &str) -> String {
    let t: String = s.chars().take(12).collect();
    if s.chars().count() > 12 { format!("{t}…") } else { t }
}

/// `G…` or `C…` as an XDR address.
pub fn sc_address(s: &str) -> Result<ScAddress, ChainError> {
    if s.starts_with('C') {
        Ok(ScAddress::Contract(ContractId(Hash(contract_bytes(s)?))))
    } else {
        Ok(ScAddress::Account(AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(account_bytes(s)?)))))
    }
}

pub fn address_string(a: &ScAddress) -> Result<String, ChainError> {
    match a {
        ScAddress::Account(AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(b)))) => Ok(account_strkey(*b)),
        ScAddress::Contract(ContractId(Hash(b))) => Ok(contract_strkey(*b)),
        other => Err(ChainError::Decode(format!("unsupported address kind: {other:?}"))),
    }
}

/// A signing identity. `Debug` shows only the public address.
pub struct Signer {
    key: SigningKey,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signer").field("address", &self.address()).finish_non_exhaustive()
    }
}

impl Signer {
    pub fn from_secret(s: &str) -> Result<Self, ChainError> {
        let sk = stellar_strkey::ed25519::PrivateKey::from_string(s).map_err(|_| ChainError::Invalid("not a Stellar secret key".into()))?;
        Ok(Self { key: SigningKey::from_bytes(&sk.0) })
    }

    pub fn public_bytes(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    pub fn address(&self) -> String {
        account_strkey(self.public_bytes())
    }

    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        self.key.sign(message).to_bytes()
    }

    /// Last four bytes of the public key: the signature hint in a transaction.
    pub fn hint(&self) -> [u8; 4] {
        let p = self.public_bytes();
        [p[28], p[29], p[30], p[31]]
    }
}

/// Verifies an ed25519 signature made by the account `address` over `message`.
pub fn verify(address: &str, message: &[u8], signature: &[u8]) -> bool {
    let (Ok(pk), Ok(sig)) = (account_bytes(address), <[u8; 64]>::try_from(signature)) else {
        return false;
    };
    let Ok(vk) = VerifyingKey::from_bytes(&pk) else {
        return false;
    };
    vk.verify_strict(message, &Signature::from_bytes(&sig)).is_ok()
}

/// Verifies a wallet's SEP-53 `signMessage` result: base64 signature over `sha256("Stellar Signed Message:\n" + message)`.
pub fn verify_sep53(address: &str, message: &str, signature_b64: &str) -> bool {
    STANDARD.decode(signature_b64.trim()).is_ok_and(|sig| verify(address, &sep53_hash(message), &sig))
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// A deterministic test identity: (secret strkey, signer).
    pub fn identity(seed: u8) -> (String, Signer) {
        let secret = stellar_strkey::Unredacted(stellar_strkey::ed25519::PrivateKey([seed; 32])).to_string();
        let signer = Signer::from_secret(&secret).unwrap();
        (secret, signer)
    }
}

#[cfg(test)]
mod tests {
    use super::testing::identity;
    use super::*;

    #[test]
    fn address_round_trip() {
        let (_, s) = identity(7);
        let g = s.address();
        assert!(g.starts_with('G') && g.len() == 56);
        assert_eq!(account_bytes(&g).unwrap(), s.public_bytes());
        assert_eq!(address_string(&sc_address(&g).unwrap()).unwrap(), g);
    }

    #[test]
    fn contract_round_trip() {
        let c = "CCEKBPYCWKNTXIQLXJHVAF7RYGUZZ6MITA7VVJBOK5BHXUO44ELZ4F4V";
        let b = contract_bytes(c).unwrap();
        assert_eq!(contract_strkey(b), c);
        assert_eq!(address_string(&sc_address(c).unwrap()).unwrap(), c);
    }

    #[test]
    fn rejects_garbage_addresses() {
        for bad in ["", "G", "GABC", "not an address", "SABC"] {
            assert!(account_bytes(bad).is_err(), "{bad}");
            assert!(!is_valid_account(bad));
        }
        // a contract id is not an account
        assert!(!is_valid_account("CCEKBPYCWKNTXIQLXJHVAF7RYGUZZ6MITA7VVJBOK5BHXUO44ELZ4F4V"));
        // a checksum typo
        let (_, s) = identity(3);
        let mut g = s.address();
        g.replace_range(10..11, if &g[10..11] == "A" { "B" } else { "A" });
        assert!(!is_valid_account(&g));
    }

    #[test]
    fn sign_and_verify() {
        let (_, s) = identity(9);
        let sig = s.sign(b"hello");
        assert!(verify(&s.address(), b"hello", &sig));
        assert!(!verify(&s.address(), b"hellp", &sig));
        assert!(!verify(&identity(10).1.address(), b"hello", &sig));
        assert!(!verify(&s.address(), b"hello", &sig[..63]));
    }

    #[test]
    fn sep53_wallet_signature() {
        let (_, s) = identity(11);
        let msg = "SkillEscrow wallet link\nNonce: abc";
        let sig = STANDARD.encode(s.sign(&sep53_hash(msg)));
        assert!(verify_sep53(&s.address(), msg, &sig));
        assert!(!verify_sep53(&s.address(), "another message", &sig));
        assert!(!verify_sep53(&s.address(), msg, "not base64!!"));
        // signing the raw message without the SEP-53 prefix must NOT verify
        let raw = STANDARD.encode(s.sign(msg.as_bytes()));
        assert!(!verify_sep53(&s.address(), msg, &raw));
    }

    #[test]
    fn signer_debug_hides_the_secret() {
        let (secret, s) = identity(5);
        let d = format!("{s:?}");
        assert!(d.contains(&s.address()));
        assert!(!d.contains(&secret));
    }

    #[test]
    fn hint_is_the_last_four_public_key_bytes() {
        let (_, s) = identity(5);
        assert_eq!(&s.hint(), &s.public_bytes()[28..]);
    }
}
