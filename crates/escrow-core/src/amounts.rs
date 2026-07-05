//! Token amounts are integers in base units (7 decimals on Stellar). Floats never touch money.

use thiserror::Error;

pub const TOKEN_DECIMALS: u32 = 7;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AmountError {
    #[error("invalid amount: {0}")]
    Invalid(String),
    #[error("too many decimals: {0}")]
    TooManyDecimals(String),
    #[error("amount is too large: {0}")]
    TooLarge(String),
}

/// `"50.5"` -> `505_000_000`. Rejects anything that is not an exact non-negative decimal.
pub fn to_base_units(s: &str) -> Result<i128, AmountError> {
    to_base_units_with(s, TOKEN_DECIMALS)
}

pub fn to_base_units_with(s: &str, decimals: u32) -> Result<i128, AmountError> {
    let t = s.trim();
    let (int, frac) = match t.split_once('.') {
        Some((i, f)) => (i, f),
        None => (t, ""),
    };
    let digits = |x: &str| !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit());
    if !digits(int) || (t.contains('.') && !digits(frac)) {
        return Err(AmountError::Invalid(s.to_string()));
    }
    if frac.len() > decimals as usize {
        return Err(AmountError::TooManyDecimals(s.to_string()));
    }
    let padded = format!("{int}{frac}{}", "0".repeat(decimals as usize - frac.len()));
    padded.parse::<i128>().map_err(|_| AmountError::TooLarge(s.to_string()))
}

/// `505_000_000` -> `"50.5"` (trailing zeros trimmed, at least `min_frac` places).
pub fn from_base_units(v: i128, decimals: u32, min_frac: usize) -> String {
    let neg = v < 0;
    let abs = v.unsigned_abs().to_string();
    let d = decimals as usize;
    let s = format!("{abs:0>width$}", width = d + 1);
    let (int, frac) = s.split_at(s.len() - d);
    let mut frac = frac.trim_end_matches('0').to_string();
    while frac.len() < min_frac {
        frac.push('0');
    }
    let sign = if neg { "-" } else { "" };
    if frac.is_empty() { format!("{sign}{int}") } else { format!("{sign}{int}.{frac}") }
}

/// Display helper used in comments: seven-decimal amounts shown with `min_frac` places.
pub fn display(v: i128, min_frac: usize) -> String {
    from_base_units(v, TOKEN_DECIMALS, min_frac)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_whole_and_fractional() {
        assert_eq!(to_base_units("50"), Ok(500_000_000));
        assert_eq!(to_base_units("50.5"), Ok(505_000_000));
        assert_eq!(to_base_units("0.0000001"), Ok(1));
        assert_eq!(to_base_units(" 7 "), Ok(70_000_000));
    }

    #[test]
    fn rejects_garbage() {
        for bad in ["", ".", "1.", ".5", "-1", "1e3", "abc", "1,5", "1.2.3", "+4", "0x10"] {
            assert!(to_base_units(bad).is_err(), "{bad} should be rejected");
        }
        assert!(matches!(to_base_units("1.12345678"), Err(AmountError::TooManyDecimals(_))));
    }

    #[test]
    fn rejects_overflow_instead_of_wrapping() {
        let huge = "9".repeat(40);
        assert!(matches!(to_base_units(&huge), Err(AmountError::TooLarge(_))));
    }

    #[test]
    fn formats_with_trimmed_zeros_and_min_places() {
        assert_eq!(from_base_units(505_000_000, 7, 0), "50.5");
        assert_eq!(from_base_units(500_000_000, 7, 0), "50");
        assert_eq!(from_base_units(500_000_000, 7, 2), "50.00");
        assert_eq!(from_base_units(1, 7, 0), "0.0000001");
        assert_eq!(from_base_units(0, 7, 2), "0.00");
        assert_eq!(from_base_units(-15_000_000, 7, 0), "-1.5");
    }

    #[test]
    fn round_trips() {
        for v in [0i128, 1, 9_999_999, 10_000_000, 123_456_789_012, i64::MAX as i128] {
            assert_eq!(to_base_units(&from_base_units(v, 7, 0)), Ok(v));
        }
    }
}
