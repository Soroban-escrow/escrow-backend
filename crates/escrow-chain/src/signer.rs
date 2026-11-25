//! Where the relayer's Ed25519 signatures come from. The local [`Signer`] is one [`SignatureProvider`];
//! [`VaultTransitSigner`] keeps the key inside HashiCorp Vault Transit, so the secret never reaches this
//! process. A cloud KMS (AWS KMS or GCP KMS both hold Ed25519 keys) is one more implementation of the trait.

use crate::{error::ChainError, keys::Signer};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, de::DeserializeOwned};
use std::{collections::HashMap, time::Duration};

const MAX_ATTEMPTS: u32 = 3;
const RETRY_BACKOFF: Duration = Duration::from_millis(250);

/// Signs transaction hashes for one Stellar account. An implementation must never return a signature
/// that the account's public key does not verify.
#[async_trait]
pub trait SignatureProvider: Send + Sync {
    /// The raw Ed25519 public key (the account's 32 bytes) that `sign` produces signatures for.
    async fn public_key(&self) -> Result<[u8; 32], ChainError>;
    /// Ed25519 signature over a 32-byte payload hash, such as a Stellar transaction hash.
    async fn sign(&self, payload_hash: [u8; 32]) -> Result<[u8; 64], ChainError>;
}

#[async_trait]
impl SignatureProvider for Signer {
    async fn public_key(&self) -> Result<[u8; 32], ChainError> {
        Ok(self.public_bytes())
    }

    async fn sign(&self, payload_hash: [u8; 32]) -> Result<[u8; 64], ChainError> {
        Ok(Signer::sign(self, &payload_hash))
    }
}

/// Signs with a Vault Transit `ed25519` key (`POST /v1/transit/sign/<key>`). The token is sent only in the
/// `X-Vault-Token` header; it never appears in `Debug` output or in an error message.
pub struct VaultTransitSigner {
    http: reqwest::Client,
    addr: String,
    key: String,
    token: String,
}

impl std::fmt::Debug for VaultTransitSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultTransitSigner").field("key", &self.key).finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct Envelope<T> {
    data: T,
}

#[derive(Deserialize)]
struct KeyRead {
    latest_version: u32,
    keys: HashMap<String, KeyVersion>,
}

#[derive(Deserialize)]
struct KeyVersion {
    public_key: String,
}

#[derive(Deserialize)]
struct SignOut {
    signature: String,
}

impl VaultTransitSigner {
    /// `addr` is the Vault base URL (`VAULT_ADDR`), `key` the transit key's name and `token` a Vault token.
    pub fn new(addr: &str, key: &str, token: String) -> Result<Self, ChainError> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(5)).build()?;
        Ok(Self { http, addr: addr.trim_end_matches('/').to_string(), key: key.to_string(), token })
    }

    /// Sends one request and decodes Vault's `{"data": …}` answer. Only transient failures are retried.
    /// Vault's own error text is never copied into the error, so nothing sensitive reaches the logs.
    async fn call<T: DeserializeOwned>(&self, build: impl Fn() -> reqwest::RequestBuilder) -> Result<T, ChainError> {
        let mut attempt = 1;
        loop {
            let result: Result<T, ChainError> = async {
                let resp = build().header("X-Vault-Token", self.token.as_str()).send().await?;
                let status = resp.status();
                if status.is_success() {
                    let body: Envelope<T> = resp.json().await?;
                    return Ok(body.data);
                }
                Err(match status.as_u16() {
                    401 | 403 => ChainError::Rejected("vault denied the request; check the token and its policy".into()),
                    429 | 500..=599 => ChainError::Http(format!("vault answered {status}")),
                    _ => ChainError::Invalid(format!("vault answered {status}")),
                })
            }
            .await;
            match result {
                Err(e) if e.is_transient() && attempt < MAX_ATTEMPTS => {
                    tokio::time::sleep(RETRY_BACKOFF * attempt).await;
                    attempt += 1;
                }
                other => return other,
            }
        }
    }
}

#[async_trait]
impl SignatureProvider for VaultTransitSigner {
    async fn public_key(&self) -> Result<[u8; 32], ChainError> {
        let url = format!("{}/v1/transit/keys/{}", self.addr, self.key);
        let read: KeyRead = self.call(|| self.http.get(url.as_str())).await?;
        let latest = read.keys.get(&read.latest_version.to_string()).ok_or_else(|| ChainError::Decode("vault key has no latest version".into()))?;
        decode_public_key(&latest.public_key)
    }

    async fn sign(&self, payload_hash: [u8; 32]) -> Result<[u8; 64], ChainError> {
        let url = format!("{}/v1/transit/sign/{}", self.addr, self.key);
        let body = serde_json::json!({ "input": STANDARD.encode(payload_hash) });
        let out: SignOut = self.call(|| self.http.post(url.as_str()).json(&body)).await?;
        let sig = parse_signature(&out.signature)?;
        // Check the answer against the key Vault reports, so a key-version mismatch or a bad reply never reaches the chain.
        let pk = self.public_key().await?;
        let vk = VerifyingKey::from_bytes(&pk).map_err(|_| ChainError::Decode("vault key is not a valid ed25519 public key".into()))?;
        vk.verify_strict(&payload_hash, &Signature::from_bytes(&sig))
            .map_err(|_| ChainError::Decode("vault signature does not verify against its key".into()))?;
        Ok(sig)
    }
}

fn decode_public_key(b64: &str) -> Result<[u8; 32], ChainError> {
    let raw = STANDARD.decode(b64).map_err(|_| ChainError::Decode("vault public key is not base64".into()))?;
    <[u8; 32]>::try_from(raw.as_slice()).map_err(|_| ChainError::Decode("vault public key is not 32 bytes".into()))
}

/// Vault answers `vault:v<version>:<base64>`.
fn parse_signature(raw: &str) -> Result<[u8; 64], ChainError> {
    let mut parts = raw.splitn(3, ':');
    let (Some("vault"), Some(_version), Some(b64)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(ChainError::Decode("vault signature has an unexpected format".into()));
    };
    let bytes = STANDARD.decode(b64).map_err(|_| ChainError::Decode("vault signature is not base64".into()))?;
    <[u8; 64]>::try_from(bytes.as_slice()).map_err(|_| ChainError::Decode("vault signature is not 64 bytes".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_vault_signature() {
        let sig = [7u8; 64];
        assert_eq!(parse_signature(&format!("vault:v1:{}", STANDARD.encode(sig))).unwrap(), sig);
    }

    #[test]
    fn rejects_malformed_vault_signatures() {
        assert!(parse_signature("garbage").is_err());
        assert!(parse_signature("other:v1:AAAA").is_err());
        assert!(parse_signature(&format!("vault:v1:{}", STANDARD.encode([1u8; 63]))).is_err());
        assert!(parse_signature("vault:v1:not base64!!").is_err());
    }

    #[test]
    fn vault_debug_output_hides_the_token_and_the_address() {
        let s = VaultTransitSigner::new("https://vault.internal:8200", "relayer", "s.SECRETTOKEN".into()).unwrap();
        let d = format!("{s:?}");
        assert!(d.contains("relayer"));
        assert!(!d.contains("SECRETTOKEN") && !d.contains("vault.internal"));
    }

    #[tokio::test]
    async fn local_signer_satisfies_the_provider_contract() {
        let (_, s) = crate::keys::testing::identity(4);
        let hash = [9u8; 32];
        let sig = SignatureProvider::sign(&s, hash).await.unwrap();
        assert!(crate::keys::verify(&s.address(), &hash, &sig));
        assert_eq!(SignatureProvider::public_key(&s).await.unwrap(), s.public_bytes());
    }
}
