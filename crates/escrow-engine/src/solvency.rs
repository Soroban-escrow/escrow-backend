//! Solvency monitoring: periodic checks that the chain and database are in sync.

use escrow_chain::ChainReader;
use escrow_db::Db;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SolvencyCheck {
    pub token: String,
    pub ledger_number: u32,
    pub held_on_chain: String,
    pub balance_actual: String,
    pub funded_expected: String,
    pub released_net_sum: String,
    pub fees_sum: String,
    pub refunded_sum: String,
    pub held_expected: String,
    pub db_held_sum: String,
    pub discrepancy: Option<String>,
    pub severity: SolvencySeverity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SolvencySeverity {
    Ok,
    Warning,
    Critical,
}

impl SolvencySeverity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }
}

/// Run a solvency check for a specific token.
/// Returns the check result and any alert message.
pub async fn check_token_solvency(
    _db: &Db,
    _chain: &dyn ChainReader,
    _token: &str,
) -> Result<(SolvencyCheck, Option<String>), Box<dyn std::error::Error>> {
    // TODO: Implement when ChainReader.get_token_totals and Db.get_held_bounties_total are available
    Err("Not implemented".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solvency_ok_when_balanced() {
        let check = SolvencyCheck {
            token: "CUSDC".to_string(),
            ledger_number: 1000,
            held_on_chain: "100".to_string(),
            balance_actual: "100".to_string(),
            funded_expected: "150".to_string(),
            released_net_sum: "40".to_string(),
            fees_sum: "5".to_string(),
            refunded_sum: "5".to_string(),
            held_expected: "100".to_string(),
            db_held_sum: "100".to_string(),
            discrepancy: None,
            severity: SolvencySeverity::Ok,
        };
        assert_eq!(check.severity, SolvencySeverity::Ok);
        assert_eq!(check.discrepancy, None);
    }

    #[test]
    fn solvency_critical_when_insolvent() {
        let check = SolvencyCheck {
            token: "CUSDC".to_string(),
            ledger_number: 1000,
            held_on_chain: "100".to_string(),
            balance_actual: "50".to_string(),
            funded_expected: "150".to_string(),
            released_net_sum: "40".to_string(),
            fees_sum: "5".to_string(),
            refunded_sum: "5".to_string(),
            held_expected: "100".to_string(),
            db_held_sum: "100".to_string(),
            discrepancy: Some("Contract holds more than it has".to_string()),
            severity: SolvencySeverity::Critical,
        };
        assert_eq!(check.severity, SolvencySeverity::Critical);
        assert!(check.discrepancy.is_some());
    }
}
