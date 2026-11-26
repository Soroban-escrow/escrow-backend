use escrow_core::contract_errors::contract_error_name;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ChainError {
    #[error("RPC error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("network error: {0}")]
    Http(String),
    #[error("simulation failed: {0}")]
    Simulation(String),
    #[error("transaction rejected: {0}")]
    Rejected(String),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("could not decode chain data: {0}")]
    Decode(String),
    #[error("timed out waiting for transaction {0}")]
    Timeout(String),
    #[error("contract state is archived and needs a restore transaction first")]
    Archived,
    #[error("the network is busy; try again shortly")]
    Busy,
}

impl ChainError {
    /// The contract's own error name (e.g. `IssueAlreadyFunded`) when the message carries one.
    /// Deterministic: retrying the same call cannot help.
    pub fn contract_error(&self) -> Option<&'static str> {
        match self {
            Self::Simulation(m) | Self::Rejected(m) | Self::Rpc { message: m, .. } => contract_error_name(m),
            _ => None,
        }
    }

    /// Worth retrying later (network trouble, congestion), as opposed to a rejection that will repeat.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Http(_) | Self::Timeout(_) | Self::Busy => true,
            // JSON-RPC: -32603 internal error and -32000..-32099 server errors may pass; -32602 invalid params will not
            Self::Rpc { code, .. } => *code == -32603 || (-32099..=-32000).contains(code) || *code == 429,
            Self::Rejected(m) => is_retryable_tx_result(m),
            _ => false,
        }
    }

    /// `true` for a rejection the relayer should never retry: a contract rejection, or a
    /// transaction result that cannot be fixed by resubmitting (insufficient balance, malformed,
    /// bad auth). Bad sequence and insufficient fee are retryable and return `false`.
    ///
    /// Non-`Rejected` variants return `false`; callers combine this with `is_transient` for
    /// network-level classification.
    pub fn is_permanent_rejection(&self) -> bool {
        if self.contract_error().is_some() {
            return true;
        }
        match self {
            Self::Rejected(m) => !is_retryable_tx_result(m),
            _ => false,
        }
    }

    /// The RPC says our cursor or start ledger is outside the data it still keeps.
    pub fn is_out_of_retention(&self) -> bool {
        let m = match self {
            Self::Rpc { message, .. } => message.to_ascii_lowercase(),
            _ => return false,
        };
        ["ledger range", "retention", "startledger", "cursor"].iter().any(|k| m.contains(k))
    }
}

/// `TransactionResult.result` names (from `describe_tx_error`) that a fresh submit may clear.
/// Bad sequence resolves when the account's seqnum is re-read; insufficient fee can be bumped on
/// the next attempt. Anything else (insufficient balance, malformed, bad auth, unknown) never will.
fn is_retryable_tx_result(result_name: &str) -> bool {
    matches!(
        result_name,
        "TxBadSeq"
            // Separate "late sequence" code in some xdr variants; same remediation.
            | "TxLateSequence"
            | "TxInsufficientFee"
    )
}

impl From<reqwest::Error> for ChainError {
    fn from(e: reqwest::Error) -> Self {
        // reqwest errors can embed the URL; the RPC URL may carry an API key, so keep only the kind.
        let kind = if e.is_timeout() {
            "timeout"
        } else if e.is_connect() {
            "connection failed"
        } else if e.is_decode() {
            "bad response body"
        } else {
            "request failed"
        };
        Self::Http(kind.to_string())
    }
}

impl From<stellar_xdr::Error> for ChainError {
    fn from(e: stellar_xdr::Error) -> Self {
        Self::Decode(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_contract_errors_from_simulation_text() {
        let e = ChainError::Simulation("HostError: Error(Contract, #4)".into());
        assert_eq!(e.contract_error(), Some("Paused"));
        assert!(!e.is_transient());
        assert_eq!(ChainError::Http("timeout".into()).contract_error(), None);
    }

    #[test]
    fn classifies_transient_failures() {
        assert!(ChainError::Http("timeout".into()).is_transient());
        assert!(ChainError::Timeout("abc".into()).is_transient());
        assert!(ChainError::Busy.is_transient());
        assert!(ChainError::Rpc { code: -32603, message: "internal".into() }.is_transient());
        assert!(!ChainError::Rpc { code: -32602, message: "invalid params".into() }.is_transient());
        // #109: a human string with spaces never matches a TxResult variant name
        assert!(!ChainError::Rejected("bad seq".into()).is_transient());
    }

    #[test]
    fn rejected_tx_results_classify_by_remediation() {
        // Retryable: re-reading the sequence or bumping the fee next attempt can clear them.
        for name in ["TxBadSeq", "TxLateSequence", "TxInsufficientFee"] {
            let e = ChainError::Rejected(name.into());
            assert!(e.is_transient(), "{name} should be transient");
            assert!(!e.is_permanent_rejection(), "{name} should not be permanent");
        }
        // Permanent: nothing we can do.
        for name in ["TxInsufficientBalance", "TxMalformed", "TxBadAuth", "TxBadAuthExtra", "TxNoAccount", "rejected"] {
            let e = ChainError::Rejected(name.into());
            assert!(!e.is_transient(), "{name} should not be transient");
            assert!(e.is_permanent_rejection(), "{name} should be permanent");
        }
    }

    #[test]
    fn contract_rejections_are_always_permanent() {
        // Even though this is a Simulation, contract_error returns Some and is_permanent_rejection
        // is the single predicate the relayer uses to decide "do not retry".
        let e = ChainError::Simulation("HostError: Error(Contract, #4)".into());
        assert!(e.is_permanent_rejection());
        // Transport failures never claim to be a permanent rejection on their own.
        assert!(!ChainError::Http("boom".into()).is_permanent_rejection());
        assert!(!ChainError::Busy.is_permanent_rejection());
    }

    #[test]
    fn detects_retention_errors() {
        let e = ChainError::Rpc { code: -32602, message: "startLedger must be within the ledger range: 100 - 200".into() };
        assert!(e.is_out_of_retention());
        assert!(!ChainError::Http("x".into()).is_out_of_retention());
    }
}
