//! Mirrors `contracts/bounty_escrow/src/fees.rs` exactly. The backend uses these only to *display*
//! and to cross-check what the chain reports; the contract is the one that moves money.

use thiserror::Error;

pub const BPS_DENOMINATOR: i128 = 10_000;
pub const MAX_FEE_BPS: u32 = 1_000;

#[derive(Debug, Error, PartialEq, Eq)]
#[error("arithmetic overflow")]
pub struct Overflow;

fn mul_div(amount: i128, bps: u32) -> Result<i128, Overflow> {
    amount.checked_mul(i128::from(bps)).map(|v| v / BPS_DENOMINATOR).ok_or(Overflow)
}

/// `floor(amount * bps / 10_000)`
pub fn fee(amount: i128, bps: u32) -> Result<i128, Overflow> {
    mul_div(amount, bps)
}

/// `amount - fee`; rounding dust goes to the receiver.
pub fn net(amount: i128, bps: u32) -> Result<i128, Overflow> {
    Ok(amount - fee(amount, bps)?)
}

/// `floor(amount * winner_bps / 10_000)`; dust stays with the maintainer.
pub fn portion(amount: i128, winner_bps: u32) -> Result<i128, Overflow> {
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
    fn rounds_down() {
        assert_eq!(fee(33, 300), Ok(0));
        assert_eq!(fee(34, 300), Ok(1));
        assert_eq!(portion(3, 3_333), Ok(0));
    }

    #[test]
    fn overflow_is_an_error_not_a_wrap() {
        assert_eq!(fee(i128::MAX, 10_000), Err(Overflow));
        assert_eq!(portion(i128::MAX, 2), Err(Overflow));
        let m = i128::MAX / 10_000;
        assert_eq!(fee(m, 10_000), Ok(m));
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
