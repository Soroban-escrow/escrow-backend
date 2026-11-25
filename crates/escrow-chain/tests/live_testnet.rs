//! Read-only checks against the real testnet contract. Ignored by default:
//!
//!   LIVE_ACCOUNT=G… LIVE_TOKEN=C… cargo test -p escrow-chain --test live_testnet -- --ignored --nocapture
//!
//! Nothing here signs or submits a transaction.

use escrow_chain::{ChainReader, ChainSettings, EventSource, FundParams, SorobanChain, TxBuilder, events::cursor_ledger};

fn chain() -> SorobanChain {
    let rpc = std::env::var("STELLAR_RPC_URL").unwrap_or_else(|_| "https://soroban-testnet.stellar.org".into());
    let contract =
        std::env::var("BOUNTY_CONTRACT_ID").unwrap_or_else(|_| "CCEKBPYCWKNTXIQLXJHVAF7RYGUZZ6MITA7VVJBOK5BHXUO44ELZ4F4V".into());
    SorobanChain::new(&ChainSettings::new(&rpc, "Test SDF Network ; September 2015", &contract)).unwrap()
}

#[tokio::test]
#[ignore = "needs the public testnet RPC"]
async fn reads_config_and_bounties() {
    let c = chain();
    let tip = c.latest_ledger().await.unwrap();
    assert!(tip > 1_000_000);
    let cfg = c.get_config().await.unwrap();
    println!("config: {cfg:?}");
    assert_eq!(cfg.fee_bps, 300);
    assert!(cfg.grace_secs > 0 && cfg.min_duration_secs < cfg.max_duration_secs);

    let missing = c.get_bounty(u64::MAX - 1).await.unwrap_err();
    assert_eq!(missing.contract_error(), Some("BountyNotFound"), "{missing}");

    for id in 1..=6 {
        match c.get_bounty(id).await {
            Ok(b) => println!("bounty {id}: {:?} {} base units, relayer_allowed={}", b.status, b.amount, b.relayer_allowed),
            Err(e) => println!("bounty {id}: {e}"),
        }
    }
}

#[tokio::test]
#[ignore = "needs the public testnet RPC"]
async fn decodes_every_real_event_in_the_retention_window() {
    let c = chain();
    let tip = c.latest_ledger().await.unwrap();
    let mut page = c.events_from_ledger(tip.saturating_sub(100_000), 200).await.unwrap();
    let (mut total, mut tracked) = (0, 0);
    for _ in 0..200 {
        for e in &page.events {
            total += 1;
            if let Some(ev) = e.decode().expect("real events must decode") {
                tracked += 1;
                println!("ledger {} {} bounty {}", e.ledger, ev.kind(), ev.bounty_id());
            }
        }
        if cursor_ledger(&page.cursor) + 1 >= page.latest_ledger {
            break;
        }
        page = c.events_from_cursor(&page.cursor, 200).await.unwrap();
    }
    println!("{total} events, {tracked} lifecycle events");
}

#[tokio::test]
#[ignore = "needs the public testnet RPC and LIVE_ACCOUNT / LIVE_TOKEN"]
async fn builds_a_real_unsigned_fund_transaction() {
    let (Ok(account), Ok(token)) = (std::env::var("LIVE_ACCOUNT"), std::env::var("LIVE_TOKEN")) else {
        panic!("set LIVE_ACCOUNT and LIVE_TOKEN");
    };
    let deadline = escrow_chain::tx::now_secs() + 14 * 86_400;
    // an issue number nobody has funded, so the simulation reaches the token transfer
    let r = chain()
        .build_fund(&FundParams {
            maintainer: account,
            repo_id: 999_000_001,
            issue: 1,
            token,
            amount: 20_000_000,
            deadline,
            relayer_allowed: true,
        })
        .await;
    match r {
        Ok(xdr) => println!("unsigned transaction ({} chars of XDR) built and simulated", xdr.len()),
        Err(e) => panic!("build failed: {e} (contract error: {:?})", e.contract_error()),
    }
}

/// Runs the relayer's whole preparation path against the REAL contract (account lookup, `release` simulated by
/// the contract, fee/resource assembly, signing), then stops. NOTHING is sent: no funds move.
///
///   RELAYER_SECRET=S… LIVE_BOUNTY=4 LIVE_WINNER=G… cargo test -p escrow-chain --test live_testnet -- --ignored --nocapture relayer
///
/// `LIVE_WINNER` must be an existing, funded account that is not the bounty's maintainer; the bounty must be open and
/// have been funded with relayer permission.
#[tokio::test]
#[ignore = "needs the public testnet RPC, RELAYER_SECRET, LIVE_BOUNTY and LIVE_WINNER"]
async fn relayer_prepares_a_real_release_without_sending_it() {
    let (Ok(secret), Ok(bounty), Ok(winner)) =
        (std::env::var("RELAYER_SECRET"), std::env::var("LIVE_BOUNTY"), std::env::var("LIVE_WINNER"))
    else {
        panic!("set RELAYER_SECRET, LIVE_BOUNTY and LIVE_WINNER");
    };
    let relayer = escrow_chain::Relayer::new(chain(), &secret).unwrap();
    let prepared = relayer
        .prepare_release(bounty.parse().unwrap(), &winner)
        .await
        .unwrap_or_else(|e| panic!("preparing failed: {e} (contract error: {:?})", e.contract_error()));
    println!("signed release ready: hash {} ({} chars of XDR); NOT sent", hex::encode(prepared.hash), prepared.envelope_b64.len());
    // and a deliberately wrong one is rejected by the contract at simulation, before any signing
    let e = relayer.prepare_release(u64::MAX - 1, &winner).await.unwrap_err();
    assert_eq!(e.contract_error(), Some("BountyNotFound"), "{e}");
}
