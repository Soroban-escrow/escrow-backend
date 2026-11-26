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

/// Property tests: the hand-written decimal arithmetic above is checked against plain integer maths over
/// random inputs, rather than only the examples in `tests`.
#[cfg(test)]
mod props {
    use super::*;
    use proptest::prelude::*;

    const SCALE: i128 = 10_i128.pow(TOKEN_DECIMALS);

    /// A fraction of 0 to 7 digits, leading and trailing zeros included (`"0"`, `"050"`, `"0000001"`).
    fn frac_digits() -> impl Strategy<Value = String> {
        proptest::collection::vec(0u8..10, 0..=TOKEN_DECIMALS as usize).prop_map(|d| d.into_iter().map(|x| char::from(b'0' + x)).collect())
    }

    /// The fraction's value in base units, computed independently of `to_base_units`.
    fn frac_value(frac: &str) -> i128 {
        frac.bytes().enumerate().map(|(i, b)| i128::from(b - b'0') * 10_i128.pow(TOKEN_DECIMALS - 1 - i as u32)).sum()
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2048))]

        /// Every decimal with up to 7 places parses to `int * 10^7 + frac`, and formatting that value
        /// gives back the canonical spelling of the input (no padding zeros, no trailing fractional zeros).
        #[test]
        fn decimal_strings_round_trip(int in any::<u64>(), frac in frac_digits(), pad in 0usize..3, dot in any::<bool>()) {
            let zeros = "0".repeat(pad);
            let s = if frac.is_empty() && !dot { format!("{zeros}{int}") } else if frac.is_empty() {
                // `"1."` is rejected by design, so a dot always comes with at least one digit after it
                format!("{zeros}{int}.0")
            } else {
                format!("{zeros}{int}.{frac}")
            };
            let expected = i128::from(int) * SCALE + frac_value(&frac);
            prop_assert_eq!(to_base_units(&s), Ok(expected), "parsing {}", s);

            let trimmed = frac.trim_end_matches('0');
            let canonical = if trimmed.is_empty() { int.to_string() } else { format!("{int}.{trimmed}") };
            prop_assert_eq!(from_base_units(expected, TOKEN_DECIMALS, 0), canonical.clone());
            prop_assert_eq!(to_base_units(&canonical), Ok(expected));
        }

        /// No input string, however malformed, makes the parser panic; it returns `Ok` or an `AmountError`.
        #[test]
        fn to_base_units_never_panics_on_arbitrary_text(s in any::<String>()) {
            let _ = to_base_units(&s);
        }

        /// Same as above, but biased toward strings that look almost like numbers, which is where the
        /// splitting, padding and overflow branches actually run.
        #[test]
        fn to_base_units_never_panics_on_number_like_text(s in "[ 0-9.+\\-eE_,]{0,60}", decimals in 0u32..=30) {
            let _ = to_base_units(&s);
            let _ = to_base_units_with(&s, decimals);
        }

        /// Whatever `from_base_units` prints for a non-negative value parses back to that exact value, for
        /// every `min_frac` that still fits in the token's decimals.
        #[test]
        fn from_base_units_output_re_parses(v in 0..=i128::MAX, min_frac in 0..=TOKEN_DECIMALS as usize) {
            let s = from_base_units(v, TOKEN_DECIMALS, min_frac);
            prop_assert_eq!(to_base_units(&s), Ok(v), "formatted as {}", s);
        }

        /// The same holds for any number of decimals, not just Stellar's 7.
        #[test]
        fn from_base_units_output_re_parses_for_any_decimals(v in 0..=i64::MAX as i128, decimals in 0u32..=18, extra in 0usize..=18) {
            let min_frac = extra.min(decimals as usize);
            let s = from_base_units(v, decimals, min_frac);
            prop_assert_eq!(to_base_units_with(&s, decimals), Ok(v), "formatted as {}", s);
        }

        /// Negative values (only ever displayed, never parsed) print as `-` followed by the positive form.
        #[test]
        fn negative_values_print_as_minus_the_positive_form(v in (i128::MIN + 1)..0, min_frac in 0..=TOKEN_DECIMALS as usize) {
            let s = from_base_units(v, TOKEN_DECIMALS, min_frac);
            let positive = s.strip_prefix('-').expect("a negative value starts with '-'");
            prop_assert_eq!(to_base_units(positive), Ok(-v));
        }
    }
}
