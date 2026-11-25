//! Mirrors `contracts/bounty_escrow/src/fees.rs` exactly. The backend uses these only to *display*
//! and to cross-check what the chain reports; the contract is the one that moves money.
//!
//! Inputs outside the contract's bounds are rejected rather than computed with: a fee above
//! [`MAX_FEE_BPS`], a winner share above [`BPS_DENOMINATOR`] and a negative amount all return an error.

use thiserror::Error;

pub const BPS_DENOMINATOR: i128 = 10_000;
pub const MAX_FEE_BPS: u32 = 1_000;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FeeError {
    #[error("arithmetic overflow")]
    Overflow,
    #[error("fee of {bps} bps is above the {} bps cap", MAX_FEE_BPS)]
    FeeBpsOutOfRange { bps: u32 },
    #[error("winner share of {bps} bps is above {} bps", BPS_DENOMINATOR)]
    WinnerBpsOutOfRange { bps: u32 },
    #[error("amount {amount} is negative")]
    NegativeAmount { amount: i128 },
}

fn check_amount(amount: i128) -> Result<(), FeeError> {
    if amount < 0 {
        return Err(FeeError::NegativeAmount { amount });
    }
    Ok(())
}

fn mul_div(amount: i128, bps: u32) -> Result<i128, FeeError> {
    amount.checked_mul(i128::from(bps)).map(|v| v / BPS_DENOMINATOR).ok_or(FeeError::Overflow)
}

/// `floor(amount * bps / 10_000)`. Errors when `bps > MAX_FEE_BPS` or `amount < 0`.
pub fn fee(amount: i128, bps: u32) -> Result<i128, FeeError> {
    check_amount(amount)?;
    if bps > MAX_FEE_BPS {
        return Err(FeeError::FeeBpsOutOfRange { bps });
    }
    mul_div(amount, bps)
}

/// `amount - fee`; rounding dust goes to the receiver. Errors on the same inputs as [`fee`].
pub fn net(amount: i128, bps: u32) -> Result<i128, FeeError> {
    Ok(amount - fee(amount, bps)?)
}

/// `floor(amount * winner_bps / 10_000)`; dust stays with the maintainer. Errors when
/// `winner_bps > BPS_DENOMINATOR` or `amount < 0`.
pub fn portion(amount: i128, winner_bps: u32) -> Result<i128, FeeError> {
    check_amount(amount)?;
    if i128::from(winner_bps) > BPS_DENOMINATOR {
        return Err(FeeError::WinnerBpsOutOfRange { bps: winner_bps });
    }
    mul_div(amount, winner_bps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_and_one() {
        assert_eq!(fee(0, 300), Ok(0));
        assert_eq!(fee(1, 300), Ok(0));
        assert_eq!(net(1, 300), Ok(1));
    }

    #[test]
    fn bps_boundaries() {
        assert_eq!(fee(1_000_000, 0), Ok(0));
        assert_eq!(fee(1_000_000, 1), Ok(100));
        assert_eq!(fee(1_000_000, 1_000), Ok(100_000));
        assert_eq!(portion(1_000_000, 10_000), Ok(1_000_000));
        assert_eq!(portion(1_000_000, 5_000), Ok(500_000));
    }

    #[test]
    fn fee_bps_just_above_the_cap_is_rejected() {
        assert_eq!(fee(1_000_000, 1_001), Err(FeeError::FeeBpsOutOfRange { bps: 1_001 }));
        assert_eq!(net(1_000_000, 1_001), Err(FeeError::FeeBpsOutOfRange { bps: 1_001 }));
        assert_eq!(fee(1_000_000, u32::MAX), Err(FeeError::FeeBpsOutOfRange { bps: u32::MAX }));
        assert_eq!(net(1_000_000, u32::MAX), Err(FeeError::FeeBpsOutOfRange { bps: u32::MAX }));
    }

    #[test]
    fn fee_bps_at_the_cap_is_accepted() {
        assert_eq!(fee(1_000_000, MAX_FEE_BPS), Ok(100_000));
        assert_eq!(net(1_000_000, MAX_FEE_BPS), Ok(900_000));
    }

    #[test]
    fn winner_bps_boundaries() {
        assert_eq!(portion(1_000_000, 0), Ok(0));
        assert_eq!(portion(1_000_000, 10_000), Ok(1_000_000));
        assert_eq!(portion(1_000_000, 10_001), Err(FeeError::WinnerBpsOutOfRange { bps: 10_001 }));
        assert_eq!(portion(1_000_000, u32::MAX), Err(FeeError::WinnerBpsOutOfRange { bps: u32::MAX }));
    }

    #[test]
    fn negative_amounts_are_rejected() {
        assert_eq!(fee(-1, 300), Err(FeeError::NegativeAmount { amount: -1 }));
        assert_eq!(net(-50_000_000, 300), Err(FeeError::NegativeAmount { amount: -50_000_000 }));
        assert_eq!(portion(-1, 5_000), Err(FeeError::NegativeAmount { amount: -1 }));
        assert_eq!(fee(i128::MIN, 0), Err(FeeError::NegativeAmount { amount: i128::MIN }));
    }

    #[test]
    fn rounds_down() {
        assert_eq!(fee(33, 300), Ok(0));
        assert_eq!(fee(34, 300), Ok(1));
        assert_eq!(portion(3, 3_333), Ok(0));
    }

    #[test]
    fn overflow_is_an_error_not_a_wrap() {
        // the largest bps the cap allows still overflows for a huge amount
        assert_eq!(fee(i128::MAX, MAX_FEE_BPS), Err(FeeError::Overflow));
        assert_eq!(portion(i128::MAX, 2), Err(FeeError::Overflow));
        let m = i128::MAX / 1_000;
        assert_eq!(fee(m, MAX_FEE_BPS), Ok(m * 1_000 / BPS_DENOMINATOR));
        assert_eq!(portion(i128::MAX / 10_000, 10_000), Ok(i128::MAX / 10_000));
    }

    #[test]
    fn net_plus_fee_is_amount_for_every_pair() {
        for a in [0i128, 1, 7, 99, 12_345_678, 5_000_000_000, 123_456_789_012_345] {
            for b in [0u32, 1, 300, 999, 1_000] {
                assert_eq!(net(a, b).unwrap() + fee(a, b).unwrap(), a);
            }
        }
    }

    #[test]
    fn the_documented_example() {
        // 5 XLM at 3%: 0.15 to the treasury, 4.85 to the contributor.
        assert_eq!(fee(50_000_000, 300), Ok(1_500_000));
        assert_eq!(net(50_000_000, 300), Ok(48_500_000));
    }
}
