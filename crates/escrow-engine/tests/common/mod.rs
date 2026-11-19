#![allow(dead_code)]
//! Shared fixtures for the engine's integration tests.

use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::Utc;
use escrow_chain::{RawEvent, keys::Signer};
use escrow_db::Db;
use escrow_engine::{
    commands::{CommandDeps, TokenTable},
    dispatch::DispatchDeps,
    indexer::IndexerDeps,
    pulls::PullDeps,
    sync::{GithubNotifier, SyncDeps, Tokens},
};
use escrow_payouts::PayoutDeps;
use escrow_testkit::{FakeChain, FakeEvents, FakeGithub};
use stellar_xdr::*;

pub const TOKEN_USDC: &str = "CUSDCTOKENADDRESS";
pub const TOKEN_XLM: &str = "CXLMTOKENADDRESS";

pub fn signer(seed: u8) -> Signer {
    let secret = stellar_strkey::Unredacted(stellar_strkey::ed25519::PrivateKey([seed; 32])).to_string();
    Signer::from_secret(&secret).unwrap()
}

/// A real, valid G… address for a seed.
pub fn addr(seed: u8) -> String {
    signer(seed).address()
}

/// A real, valid C… token address.
pub const TOKEN_C: &str = "CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC";

fn sym(s: &str) -> ScVal {
    ScVal::Symbol(ScSymbol::try_from(s).unwrap())
}
fn a(s: &str) -> ScVal {
    ScVal::Address(s.parse().unwrap())
}
fn i128v(v: i128) -> ScVal {
    ScVal::I128(Int128Parts { hi: (v >> 64) as i64, lo: v as u64 })
}
fn map(pairs: Vec<(&str, ScVal)>) -> ScVal {
    ScVal::Map(Some(ScMap::sorted_from(pairs.into_iter().map(|(k, v)| ScMapEntry { key: sym(k), val: v }).collect::<Vec<_>>()).unwrap()))
}
fn b64(v: &ScVal) -> String {
    STANDARD.encode(v.to_xdr(Limits::none()).unwrap())
}

/// Event ids sort like the chain's: zero-padded ledger/op.
pub fn event_id(ledger: u32, n: u32) -> String {
    format!("{:019}-{:010}", u64::from(ledger) << 32, n)
}

fn raw(name: &str, id: u64, value: ScVal, ledger: u32, n: u32) -> RawEvent {
    RawEvent {
        id: event_id(ledger, n),
        ledger,
        tx_hash: format!("{ledger:02x}{n:02x}").repeat(16)[..64].to_string(),
        ledger_closed_at: "2026-11-03T10:00:00Z".into(),
        topics_b64: vec![b64(&sym("bounty")), b64(&sym(name)), b64(&ScVal::U64(id))],
        value_b64: b64(&value),
        in_successful_contract_call: true,
    }
}

pub struct Funded<'a> {
    pub id: u64,
    pub maintainer: &'a str,
    pub repo_id: u64,
    pub issue: u64,
    pub token: &'a str,
    pub amount: i128,
    pub relayer_allowed: bool,
}

pub fn funded(f: &Funded<'_>, ledger: u32, n: u32) -> RawEvent {
    raw(
        "funded",
        f.id,
        map(vec![
            ("maintainer", a(f.maintainer)),
            ("repo_id", ScVal::U64(f.repo_id)),
            ("issue", ScVal::U64(f.issue)),
            ("token", a(f.token)),
            ("amount", i128v(f.amount)),
            ("fee_bps", ScVal::U32(300)),
            ("deadline", ScVal::U64(1_795_000_000)),
            ("relayer_allowed", ScVal::Bool(f.relayer_allowed)),
        ]),
        ledger,
        n,
    )
}

pub fn released(id: u64, winner: &str, caller: &str, ledger: u32, n: u32) -> RawEvent {
    raw(
        "released",
        id,
        map(vec![("winner", a(winner)), ("net", i128v(48_500_000)), ("fee", i128v(1_500_000)), ("caller", a(caller))]),
        ledger,
        n,
    )
}
pub fn refunded(id: u64, ledger: u32, n: u32) -> RawEvent {
    raw("refunded", id, map(vec![("amount", i128v(50_000_000))]), ledger, n)
}
pub fn disputed(id: u64, caller: &str, ledger: u32, n: u32) -> RawEvent {
    raw("disputed", id, map(vec![("caller", a(caller))]), ledger, n)
}
pub fn resolved(id: u64, winner: Option<&str>, ledger: u32, n: u32) -> RawEvent {
    raw(
        "resolved",
        id,
        map(vec![("winner", winner.map_or(ScVal::Void, a)), ("winner_net", i128v(1)), ("fee", i128v(0)), ("maintainer_amount", i128v(1))]),
        ledger,
        n,
    )
}
pub fn paused_event(ledger: u32, n: u32) -> RawEvent {
    let mut e = raw("paused", 0, map(vec![]), ledger, n);
    e.topics_b64.truncate(2);
    e
}

pub struct World {
    pub db: Db,
    pub gh: std::sync::Arc<FakeGithub>,
    pub chain: std::sync::Arc<FakeChain>,
    pub events: std::sync::Arc<FakeEvents>,
}

impl World {
    pub fn new(pool: sqlx::PgPool) -> Self {
        let w = Self {
            db: Db::from_pool(pool),
            gh: std::sync::Arc::new(FakeGithub::new()),
            chain: std::sync::Arc::new(FakeChain::default()),
            events: std::sync::Arc::new(FakeEvents::default()),
        };
        *w.chain.ledger.lock().unwrap() = 10_000;
        w
    }

    pub fn indexer(&self) -> IndexerDeps {
        IndexerDeps { db: self.db.clone(), source: self.events.clone(), chain: self.chain.clone(), start_ledger: None, lookback: 1000 }
    }

    pub fn payouts(&self) -> PayoutDeps {
        PayoutDeps {
            db: self.db.clone(),
            chain: self.chain.clone(),
            notifier: std::sync::Arc::new(GithubNotifier { db: self.db.clone(), gh: self.gh.clone() }),
            web_url: "https://app.example".into(),
        }
    }

    pub fn sync(&self) -> SyncDeps {
        SyncDeps {
            db: self.db.clone(),
            gh: self.gh.clone(),
            web_url: "https://app.example".into(),
            tokens: Tokens { usdc: TOKEN_USDC.into(), xlm: TOKEN_XLM.into() },
            explorer_template: "https://explorer.example/tx/{hash}".into(),
        }
    }

    pub fn commands(&self, reviews: Option<std::sync::Arc<dyn escrow_engine::ports::ReviewScheduler>>) -> CommandDeps {
        CommandDeps {
            db: self.db.clone(),
            gh: self.gh.clone(),
            web_url: "https://app.example".into(),
            tokens: TokenTable { usdc: Some(TOKEN_USDC.into()), xlm: Some(TOKEN_XLM.into()), min: 10_000_000, max: 5_000_000_000 },
            min_days: 1,
            max_days: 90,
            reviews,
            now: Utc::now,
        }
    }

    pub fn pulls(&self, reviews: Option<std::sync::Arc<dyn escrow_engine::ports::ReviewScheduler>>) -> PullDeps {
        PullDeps { db: self.db.clone(), payouts: self.payouts(), reviews, review_delay: std::time::Duration::from_secs(300) }
    }

    pub fn dispatch(&self, reviews: Option<std::sync::Arc<dyn escrow_engine::ports::ReviewScheduler>>) -> DispatchDeps {
        DispatchDeps { db: self.db.clone(), commands: self.commands(reviews.clone()), pulls: self.pulls(reviews) }
    }
}

/// Records review scheduling requests.
#[derive(Default)]
pub struct Scheduler(pub std::sync::Mutex<Vec<(i64, std::time::Duration, bool)>>);

#[async_trait::async_trait]
impl escrow_engine::ports::ReviewScheduler for Scheduler {
    async fn schedule(&self, pr_id: i64, delay: std::time::Duration, force: bool) -> Result<(), String> {
        self.0.lock().unwrap().push((pr_id, delay, force));
        Ok(())
    }
}
