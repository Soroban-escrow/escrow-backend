//! Soroban RPC over JSON-RPC 2.0: the wire shapes and a small client.

use crate::{error::ChainError, events::decode_scval};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::time::Duration;
use stellar_xdr::{Limits, ReadXdr, ScVal, SorobanAuthorizationEntry, SorobanTransactionData};

#[derive(Clone)]
pub struct RpcClient {
    http: reqwest::Client,
    url: String,
}

#[derive(Deserialize)]
struct Reply<T> {
    result: Option<T>,
    error: Option<RpcFault>,
}

#[derive(Deserialize)]
struct RpcFault {
    code: i64,
    message: String,
}

impl RpcClient {
    pub fn new(url: &str, timeout: Duration) -> Result<Self, ChainError> {
        let http = reqwest::Client::builder().timeout(timeout).user_agent("skillescrow-backend").build().map_err(ChainError::from)?;
        Ok(Self { http, url: url.to_string() })
    }

    pub async fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T, ChainError> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let resp = self.http.post(&self.url).json(&body).send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        let reply: Reply<T> =
            serde_json::from_str(&text).map_err(|_| ChainError::Http(format!("{method}: unexpected response (HTTP {status})")))?;
        if let Some(f) = reply.error {
            return Err(ChainError::Rpc { code: f.code, message: f.message });
        }
        reply.result.ok_or_else(|| ChainError::Decode(format!("{method}: empty result")))
    }
}

// ---- wire types ----

#[derive(Deserialize)]
pub struct LatestLedgerWire {
    pub sequence: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventsWire {
    #[serde(default)]
    pub events: Vec<EventWire>,
    pub cursor: Option<String>,
    pub latest_ledger: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventWire {
    pub id: String,
    pub ledger: u32,
    #[serde(default)]
    pub ledger_closed_at: String,
    #[serde(default)]
    pub tx_hash: String,
    #[serde(default)]
    pub topic: Vec<String>,
    pub value: String,
    #[serde(default = "yes")]
    pub in_successful_contract_call: bool,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SimulateWire {
    pub error: Option<String>,
    pub transaction_data: Option<String>,
    pub min_resource_fee: Option<String>,
    pub results: Option<Vec<SimResultWire>>,
    pub restore_preamble: Option<Value>,
}

#[derive(Deserialize)]
pub struct SimResultWire {
    pub auth: Option<Vec<String>>,
    pub xdr: Option<String>,
}

/// A simulation that succeeded, decoded.
#[derive(Debug, Clone)]
pub struct SimulationOk {
    pub transaction_data: SorobanTransactionData,
    pub min_resource_fee: i64,
    pub auth: Vec<SorobanAuthorizationEntry>,
    /// the call's return value (read-only calls use this)
    pub result: Option<ScVal>,
}

impl SimulateWire {
    pub fn into_ok(self) -> Result<SimulationOk, ChainError> {
        if let Some(e) = self.error {
            return Err(ChainError::Simulation(e));
        }
        if self.restore_preamble.is_some() {
            return Err(ChainError::Archived);
        }
        let data = self.transaction_data.ok_or_else(|| ChainError::Decode("simulation had no transactionData".into()))?;
        let transaction_data = SorobanTransactionData::from_xdr_base64(data, Limits::none())?;
        let min_resource_fee =
            self.min_resource_fee.as_deref().unwrap_or("0").parse::<i64>().map_err(|_| ChainError::Decode("bad minResourceFee".into()))?;
        let first = self.results.and_then(|r| r.into_iter().next());
        let auth = first
            .as_ref()
            .and_then(|r| r.auth.as_ref())
            .map(|a| {
                a.iter()
                    .map(|x| SorobanAuthorizationEntry::from_xdr_base64(x, Limits::none()).map_err(ChainError::from))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();
        let result = first.and_then(|r| r.xdr).map(|x| decode_scval(&x)).transpose()?;
        Ok(SimulationOk { transaction_data, min_resource_fee, auth, result })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendWire {
    pub status: String,
    #[serde(default)]
    pub hash: String,
    pub error_result_xdr: Option<String>,
}

#[derive(Deserialize)]
pub struct GetTxWire {
    pub status: String,
}

#[derive(Deserialize)]
pub struct LedgerEntriesWire {
    pub entries: Option<Vec<LedgerEntryWire>>,
}

#[derive(Deserialize)]
pub struct LedgerEntryWire {
    pub xdr: String,
}

/// Human-readable reason from a `TransactionResult` (e.g. `TxBadSeq`), when it can be decoded.
pub fn describe_tx_error(error_result_b64: Option<&str>) -> String {
    let Some(b64) = error_result_b64 else {
        return "rejected".into();
    };
    STANDARD
        .decode(b64)
        .ok()
        .and_then(|b| stellar_xdr::TransactionResult::from_xdr(b, Limits::none()).ok())
        .map_or_else(|| "rejected".into(), |r| r.result.name().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::WriteXdr;

    fn b64<T: WriteXdr>(v: &T) -> String {
        v.to_xdr_base64(Limits::none()).unwrap()
    }

    #[test]
    fn a_failed_simulation_keeps_the_contract_error() {
        let w: SimulateWire = serde_json::from_value(json!({ "error": "HostError: Error(Contract, #9)" })).unwrap();
        let e = w.into_ok().unwrap_err();
        assert_eq!(e.contract_error(), Some("IssueAlreadyFunded"));
    }

    #[test]
    fn archived_state_is_reported_not_papered_over() {
        let w: SimulateWire = serde_json::from_value(json!({ "restorePreamble": { "minResourceFee": "1" } })).unwrap();
        assert!(matches!(w.into_ok(), Err(ChainError::Archived)));
    }

    #[test]
    fn a_good_simulation_decodes_data_fee_auth_and_result() {
        let data = SorobanTransactionData::default();
        let w: SimulateWire = serde_json::from_value(json!({
            "transactionData": b64(&data),
            "minResourceFee": "58181",
            "results": [{ "auth": [], "xdr": b64(&ScVal::U64(7)) }]
        }))
        .unwrap();
        let ok = w.into_ok().unwrap();
        assert_eq!(ok.min_resource_fee, 58181);
        assert_eq!(ok.result, Some(ScVal::U64(7)));
        assert!(ok.auth.is_empty());
    }

    #[test]
    fn corrupt_simulation_data_is_an_error() {
        let w: SimulateWire = serde_json::from_value(json!({ "transactionData": "!!!", "minResourceFee": "1" })).unwrap();
        assert!(w.into_ok().is_err());
        let w: SimulateWire = serde_json::from_value(json!({})).unwrap();
        assert!(w.into_ok().is_err());
    }

    #[test]
    fn events_wire_tolerates_missing_optional_fields() {
        let w: EventsWire = serde_json::from_value(json!({
            "latestLedger": 10, "cursor": "c",
            "events": [{ "id": "1-1", "ledger": 5, "value": "AAAA" }]
        }))
        .unwrap();
        assert!(w.events[0].in_successful_contract_call);
        assert!(w.events[0].topic.is_empty());
        let none: EventsWire = serde_json::from_value(json!({ "latestLedger": 1 })).unwrap();
        assert!(none.events.is_empty() && none.cursor.is_none());
    }

    #[test]
    fn describes_known_and_unknown_tx_errors() {
        assert_eq!(describe_tx_error(None), "rejected");
        assert_eq!(describe_tx_error(Some("not base64 xdr")), "rejected");
        let r = stellar_xdr::TransactionResult {
            fee_charged: 100,
            result: stellar_xdr::TransactionResultResult::TxBadSeq,
            ext: stellar_xdr::TransactionResultExt::V0,
        };
        assert_eq!(describe_tx_error(Some(&b64(&r))), "TxBadSeq");
    }
}
