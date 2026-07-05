//! Mirrors `contracts/bounty_escrow/src/errors.rs`. The numbering is part of the contract's ABI and is never renumbered.

use regex::Regex;
use std::sync::LazyLock;

pub const CONTRACT_ERRORS: [(u32, &str); 17] = [
    (1, "AlreadyInitialized"),
    (2, "NotInitialized"),
    (3, "Unauthorized"),
    (4, "Paused"),
    (5, "TokenNotAllowed"),
    (6, "AmountTooSmall"),
    (7, "AmountTooLarge"),
    (8, "InvalidDeadline"),
    (9, "IssueAlreadyFunded"),
    (10, "BountyNotFound"),
    (11, "InvalidStatus"),
    (12, "DeadlineNotReached"),
    (13, "SelfPayout"),
    (14, "InvalidSplit"),
    (15, "InvalidFee"),
    (16, "Overflow"),
    (17, "RelayerNotAllowed"),
];

static CONTRACT_ERR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"Error\(Contract, #(\d+)\)").unwrap());

pub fn name_for_code(code: u32) -> Option<&'static str> {
    CONTRACT_ERRORS.iter().find(|(c, _)| *c == code).map(|(_, n)| *n)
}

/// Extracts `Error(Contract, #N)` from a simulation or RPC message and names it.
pub fn contract_error_name(message: &str) -> Option<&'static str> {
    let code: u32 = CONTRACT_ERR.captures(message)?.get(1)?.as_str().parse().ok()?;
    name_for_code(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_error_inside_a_simulation_message() {
        let msg = "HostError: Error(Contract, #9)\n\nEvent log (newest first): ...";
        assert_eq!(contract_error_name(msg), Some("IssueAlreadyFunded"));
        assert_eq!(contract_error_name("Error(Contract, #17)"), Some("RelayerNotAllowed"));
    }

    #[test]
    fn ignores_unknown_and_unrelated_messages() {
        assert_eq!(contract_error_name("Error(Contract, #999)"), None);
        assert_eq!(contract_error_name("network timeout"), None);
        assert_eq!(contract_error_name("Error(WasmVm, InvalidAction)"), None);
    }

    #[test]
    fn numbering_is_dense_and_stable() {
        for (i, (code, _)) in CONTRACT_ERRORS.iter().enumerate() {
            assert_eq!(*code as usize, i + 1);
        }
    }
}
