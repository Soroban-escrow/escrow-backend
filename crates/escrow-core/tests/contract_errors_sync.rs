//! Guards `CONTRACT_ERRORS` against drift from the contract's `errors.rs`.
//!
//! The contract file is vendored at the commit named in `fixtures/bounty_escrow_errors.commit`.
//! To bump it, see "Contract errors" in docs/DEVELOPMENT.md.

use escrow_core::contract_errors::CONTRACT_ERRORS;
use std::collections::BTreeMap;

const VENDORED: &str = include_str!("fixtures/bounty_escrow_errors.rs.txt");
const COMMIT: &str = include_str!("fixtures/bounty_escrow_errors.commit");

/// Reads `Name = 12,` variants out of the contract's `Error` enum.
fn parse_variants(source: &str) -> BTreeMap<u32, String> {
    let mut out = BTreeMap::new();
    let mut in_enum = false;
    for line in source.lines().map(str::trim) {
        if line.starts_with("pub enum Error") {
            in_enum = true;
        } else if in_enum && line.starts_with('}') {
            break;
        } else if in_enum
            && !line.starts_with("//")
            && let Some((name, code)) = line.trim_end_matches(',').split_once('=')
            && let Ok(code) = code.trim().parse::<u32>()
        {
            assert!(out.insert(code, name.trim().to_string()).is_none(), "contract reuses error code {code}");
        }
    }
    out
}

/// A readable report of every difference, or `None` when both sides agree.
fn diff(contract: &BTreeMap<u32, String>, ours: &BTreeMap<u32, String>) -> Option<String> {
    let mut lines = vec![];
    for (code, name) in contract {
        match ours.get(code) {
            None => lines.push(format!("  + #{code} {name}: in the contract, missing from CONTRACT_ERRORS")),
            Some(n) if n != name => lines.push(format!("  ~ #{code}: contract says {name}, CONTRACT_ERRORS says {n}")),
            _ => {}
        }
    }
    for (code, name) in ours {
        if !contract.contains_key(code) {
            lines.push(format!("  - #{code} {name}: in CONTRACT_ERRORS, not in the contract"));
        }
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

#[test]
fn contract_errors_match_the_pinned_contract() {
    let contract = parse_variants(VENDORED);
    assert!(!contract.is_empty(), "no variants parsed from the vendored errors.rs");
    let ours: BTreeMap<u32, String> = CONTRACT_ERRORS.iter().map(|(c, n, _)| (*c, n.to_string())).collect();
    if let Some(d) = diff(&contract, &ours) {
        panic!(
            "CONTRACT_ERRORS has drifted from escrow-contracts at {}:\n{d}\n\nUpdate crates/escrow-core/src/contract_errors.rs (see docs/DEVELOPMENT.md, \"Contract errors\").",
            COMMIT.trim()
        );
    }
}

#[test]
fn the_pin_is_a_full_commit_sha() {
    let c = COMMIT.trim();
    assert!(c.len() == 40 && c.bytes().all(|b| b.is_ascii_hexdigit()), "bad pin: {c:?}");
}

#[test]
fn the_parser_reads_variants_and_skips_comments() {
    let src = "pub enum Error {\n    A = 1,\n    /// doc\n    B = 2,\n}\nstruct X { C = 3, }";
    assert_eq!(parse_variants(src), BTreeMap::from([(1, "A".to_string()), (2, "B".to_string())]));
}

#[test]
fn the_diff_names_missing_renamed_and_extra_codes() {
    let contract = BTreeMap::from([(1, "A".to_string()), (2, "B".to_string()), (3, "C".to_string())]);
    let ours = BTreeMap::from([(1, "A".to_string()), (2, "Renamed".to_string()), (4, "Gone".to_string())]);
    let d = diff(&contract, &ours).unwrap();
    assert!(d.contains("+ #3 C") && d.contains("~ #2: contract says B, CONTRACT_ERRORS says Renamed") && d.contains("- #4 Gone"), "{d}");
    assert_eq!(diff(&contract, &contract), None);
}
