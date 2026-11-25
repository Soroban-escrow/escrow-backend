//! Architecture guard rails, enforced from `cargo metadata`. These are the rules the design depends on;
//! if one fails, the fix is almost never to edit the test.

use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap},
    process::Command,
};

/// package name -> names of the workspace-or-external packages it depends on (normal dependencies only)
fn graph() -> HashMap<String, BTreeSet<String>> {
    let out = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--locked"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo metadata runs");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let meta: Value = serde_json::from_slice(&out.stdout).unwrap();
    let names: HashMap<String, String> = meta["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["id"].as_str().unwrap().to_string(), p["name"].as_str().unwrap().to_string()))
        .collect();
    meta["resolve"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| {
            let deps = n["deps"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|d| d["dep_kinds"].as_array().unwrap().iter().any(|k| k["kind"].is_null())) // normal deps, not dev/build
                .map(|d| names[d["pkg"].as_str().unwrap()].clone())
                .collect();
            (names[n["id"].as_str().unwrap()].clone(), deps)
        })
        .collect()
}

fn closure(g: &HashMap<String, BTreeSet<String>>, root: &str) -> BTreeSet<String> {
    let (mut seen, mut stack) = (BTreeSet::new(), vec![root.to_string()]);
    while let Some(n) = stack.pop() {
        for d in g.get(&n).into_iter().flatten() {
            if seen.insert(d.clone()) {
                stack.push(d.clone());
            }
        }
    }
    seen
}

#[test]
fn nothing_on_the_money_path_can_reach_the_ai_reviewer() {
    let g = graph();
    for money in ["escrow-core", "escrow-chain", "escrow-db", "escrow-payouts", "escrow-engine", "escrow-api", "escrow-github"] {
        let reach = closure(&g, money);
        assert!(!reach.contains("escrow-ai"), "{money} depends (transitively) on escrow-ai: an AI review could influence a payout");
    }
}

#[test]
fn the_ai_reviewer_cannot_reach_anything_that_moves_money() {
    let g = graph();
    let reach = closure(&g, "escrow-ai");
    // (generic crypto such as ed25519-dalek is allowed: jsonwebtoken pulls it in to sign the GitHub App JWT. What matters is
    // that nothing here can build or sign a *Stellar* transaction: that needs stellar-xdr, which stays out of reach.)
    for forbidden in ["escrow-payouts", "escrow-chain", "escrow-engine", "escrow-api", "stellar-xdr", "stellar-strkey"] {
        assert!(!reach.contains(forbidden), "escrow-ai can reach {forbidden}: it must only be able to read PRs and write comments");
    }
}

#[test]
fn the_core_crate_is_pure() {
    let g = graph();
    let reach = closure(&g, "escrow-core");
    for impure in ["tokio", "reqwest", "sqlx", "hyper", "axum", "async-std", "mio"] {
        assert!(
            !reach.contains(impure),
            "escrow-core pulls in {impure}: it must stay free of I/O so the money rules are testable in isolation"
        );
    }
}

#[test]
fn only_the_binary_links_everything() {
    let g = graph();
    for lib in ["escrow-core", "escrow-chain", "escrow-db", "escrow-github", "escrow-payouts", "escrow-ai", "escrow-engine", "escrow-api"] {
        assert!(!closure(&g, lib).contains("escrowd"), "{lib} must not depend on the binary");
    }
    let bin = &g["escrowd"];
    for needed in ["escrow-ai", "escrow-engine", "escrow-api", "escrow-payouts"] {
        assert!(bin.contains(needed), "escrowd should wire in {needed}");
    }
}

#[test]
fn the_testkit_never_ships() {
    let g = graph();
    for prod in ["escrowd", "escrow-api", "escrow-engine", "escrow-ai", "escrow-payouts"] {
        assert!(!closure(&g, prod).contains("escrow-testkit"), "{prod} must not depend on the test fakes outside of dev-dependencies");
    }
}

#[test]
fn the_database_layer_is_independent_of_the_network() {
    let g = graph();
    let reach = closure(&g, "escrow-db");
    for net in ["reqwest", "escrow-chain", "escrow-github", "axum"] {
        assert!(!reach.contains(net), "escrow-db reaches {net}");
    }
}
