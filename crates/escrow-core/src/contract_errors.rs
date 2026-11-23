//! Mirrors `contracts/bounty_escrow/src/errors.rs`. The numbering is part of the contract's ABI and is never renumbered.

use regex::Regex;
use std::sync::LazyLock;

/// `(code, variant name, message for people)`. Clients branch on the name; the message is shown as is.
pub const CONTRACT_ERRORS: [(u32, &str, &str); 26] = [
    (1, "AlreadyInitialized", "The escrow contract has already been initialized."),
    (2, "NotInitialized", "The escrow contract has not been initialized yet."),
    (3, "Unauthorized", "This account is not allowed to perform this action."),
    (4, "Paused", "The escrow contract is paused right now. Please try again later."),
    (5, "TokenNotAllowed", "This token is not accepted for bounties."),
    (6, "AmountTooSmall", "The amount is below the minimum allowed for this token."),
    (7, "AmountTooLarge", "The amount is above the maximum allowed for this token."),
    (8, "InvalidDeadline", "The deadline is not valid. Choose a date further in the future."),
    (9, "IssueAlreadyFunded", "This issue already has a bounty."),
    (10, "BountyNotFound", "No bounty was found for this issue."),
    (11, "InvalidStatus", "The bounty is not in a state that allows this action."),
    (12, "DeadlineNotReached", "The deadline and grace period have not passed yet, so a refund is not possible."),
    (13, "SelfPayout", "The maintainer cannot be the winner of their own bounty."),
    (14, "InvalidSplit", "The payout split is not valid."),
    (15, "InvalidFee", "The fee is not valid."),
    (16, "Overflow", "The amount is too large to process."),
    (17, "RelayerNotAllowed", "This bounty was funded without relayer permission, so the maintainer must sign the release."),
    (18, "TopUpLimitReached", "This bounty cannot be topped up any further."),
    (19, "DisputeNotExpired", "The dispute window has not expired yet."),
    (20, "DisputeAlreadyUsed", "A dispute was already raised for this bounty."),
    (21, "NothingToClaim", "There is nothing to claim for this account."),
    (22, "AlreadyClaimed", "This has already been claimed."),
    (23, "ConflictOfInterest", "The winner cannot be the arbiter or the treasury."),
    (24, "UnexpectedTransferAmount", "The token credited a different amount than requested, so the transfer was refused."),
    (25, "NoOpAdminChange", "The new admin is the same as the current admin."),
    (26, "NothingToSweep", "There is no excess balance to sweep."),
];

static CONTRACT_ERR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"Error\(Contract, #(\d+)\)").unwrap());

pub fn name_for_code(code: u32) -> Option<&'static str> {
    CONTRACT_ERRORS.iter().find(|(c, ..)| *c == code).map(|(_, n, _)| *n)
}

/// The human-readable message for a variant name from [`CONTRACT_ERRORS`].
pub fn message_for_name(name: &str) -> Option<&'static str> {
    CONTRACT_ERRORS.iter().find(|(_, n, _)| *n == name).map(|(.., m)| *m)
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
    fn every_error_has_a_distinct_name_and_a_message() {
        let mut names: Vec<_> = CONTRACT_ERRORS.iter().map(|(_, n, _)| *n).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), CONTRACT_ERRORS.len());
        for (_, name, msg) in CONTRACT_ERRORS {
            assert!(msg.len() > 10 && msg.ends_with('.') && msg != name, "{name}: {msg}");
            assert_eq!(message_for_name(name), Some(msg));
        }
        assert_eq!(message_for_name("Nope"), None);
    }

    #[test]
    fn numbering_is_dense_and_stable() {
        for (i, (code, ..)) in CONTRACT_ERRORS.iter().enumerate() {
            assert_eq!(*code as usize, i + 1);
        }
    }
}