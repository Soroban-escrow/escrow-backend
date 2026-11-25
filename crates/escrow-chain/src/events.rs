//! Decoding of the contract's events (see `contracts/bounty_escrow/src/events.rs`).
//!
//! Only the five lifecycle events change what we show; `config`, `paused`, `unpaused` and `upgraded`
//! are recognised and ignored. A *tracked* event that is malformed is an error, never a silent skip:
//! the indexer would rather stop and alert than index something wrong.

use crate::{
    error::ChainError,
    scval::{Fields, read_symbol, read_u64},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use stellar_xdr::{Limits, ReadXdr, ScVal};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainEvent {
    Funded {
        id: u64,
        maintainer: String,
        repo_id: u64,
        issue: u64,
        token: String,
        amount: i128,
        fee_bps: u32,
        deadline: u64,
        relayer_allowed: bool,
    },
    Released {
        id: u64,
        winner: String,
        net: i128,
        fee: i128,
        caller: String,
    },
    Refunded {
        id: u64,
        amount: i128,
    },
    Disputed {
        id: u64,
        caller: String,
    },
    Resolved {
        id: u64,
        winner: Option<String>,
        winner_net: i128,
        fee: i128,
        maintainer_amount: i128,
    },
}

impl ChainEvent {
    pub fn bounty_id(&self) -> u64 {
        match self {
            Self::Funded { id, .. }
            | Self::Released { id, .. }
            | Self::Refunded { id, .. }
            | Self::Disputed { id, .. }
            | Self::Resolved { id, .. } => *id,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Funded { .. } => "funded",
            Self::Released { .. } => "released",
            Self::Refunded { .. } => "refunded",
            Self::Disputed { .. } => "disputed",
            Self::Resolved { .. } => "resolved",
        }
    }
}

/// One event as the RPC returns it.
#[derive(Debug, Clone)]
pub struct RawEvent {
    /// Ordered and zero-padded: plain string comparison gives chain order.
    pub id: String,
    pub ledger: u32,
    pub tx_hash: String,
    pub ledger_closed_at: String,
    pub topics_b64: Vec<String>,
    pub value_b64: String,
    pub in_successful_contract_call: bool,
}

impl RawEvent {
    pub fn decode(&self) -> Result<Option<ChainEvent>, ChainError> {
        let topics = self.topics_b64.iter().map(|t| decode_scval(t)).collect::<Result<Vec<_>, _>>()?;
        decode_event(&topics, &decode_scval(&self.value_b64)?)
    }
}

pub fn decode_scval(b64: &str) -> Result<ScVal, ChainError> {
    let bytes = STANDARD.decode(b64).map_err(|e| ChainError::Decode(format!("bad base64: {e}")))?;
    Ok(ScVal::from_xdr(bytes, Limits::none())?)
}

/// `Ok(None)` for events we do not track; `Err` for a tracked event whose shape is wrong.
pub fn decode_event(topics: &[ScVal], value: &ScVal) -> Result<Option<ChainEvent>, ChainError> {
    let (Some(ScVal::Symbol(ns)), Some(name)) = (topics.first(), topics.get(1)) else {
        return Ok(None);
    };
    if ns.to_utf8_string_lossy() != "bounty" {
        return Ok(None);
    }
    let Ok(name) = read_symbol(name) else {
        return Ok(None);
    };
    if !["funded", "released", "refunded", "disputed", "resolved"].contains(&name.as_str()) {
        return Ok(None);
    }
    let id = topics
        .get(2)
        .ok_or_else(|| ChainError::Decode(format!("event {name}: missing bounty id topic")))
        .and_then(read_u64)
        .map_err(|e| ChainError::Decode(format!("event {name}: {e}")))?;
    let f = Fields::of(value).map_err(|e| ChainError::Decode(format!("event {name}: {e}")))?;
    let wrap = |e: ChainError| ChainError::Decode(format!("event {name}: {e}"));

    Ok(Some(match name.as_str() {
        "funded" => ChainEvent::Funded {
            id,
            maintainer: f.address("maintainer").map_err(wrap)?,
            repo_id: f.u64("repo_id").map_err(wrap)?,
            issue: f.u64("issue").map_err(wrap)?,
            token: f.address("token").map_err(wrap)?,
            amount: f.i128("amount").map_err(wrap)?,
            fee_bps: f.u32("fee_bps").map_err(wrap)?,
            deadline: f.u64("deadline").map_err(wrap)?,
            relayer_allowed: f.bool("relayer_allowed").map_err(wrap)?,
        },
        "released" => ChainEvent::Released {
            id,
            winner: f.address("winner").map_err(wrap)?,
            net: f.i128("net").map_err(wrap)?,
            fee: f.i128("fee").map_err(wrap)?,
            caller: f.address("caller").map_err(wrap)?,
        },
        "refunded" => ChainEvent::Refunded { id, amount: f.i128("amount").map_err(wrap)? },
        "disputed" => ChainEvent::Disputed { id, caller: f.address("caller").map_err(wrap)? },
        _ => ChainEvent::Resolved {
            id,
            winner: f.opt_address("winner").map_err(wrap)?,
            winner_net: f.i128("winner_net").map_err(wrap)?,
            fee: f.i128("fee").map_err(wrap)?,
            maintainer_amount: f.i128("maintainer_amount").map_err(wrap)?,
        },
    }))
}

/// The ledger sequence inside an RPC cursor (a TOID: the ledger is the high 32 bits).
pub fn cursor_ledger(cursor: &str) -> u32 {
    cursor.split('-').next().and_then(|s| s.parse::<u64>().ok()).map_or(0, |toid| (toid >> 32) as u32)
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::scval::*;

    pub fn topics(name: &str, id: Option<u64>) -> Vec<ScVal> {
        let mut t = vec![symbol("bounty").unwrap(), symbol(name).unwrap()];
        if let Some(id) = id {
            t.push(u64v(id));
        }
        t
    }

    pub fn funded_value(maintainer: &str, token: &str) -> ScVal {
        map_of(vec![
            ("maintainer", address(maintainer).unwrap()),
            ("repo_id", u64v(1_101_352_203)),
            ("issue", u64v(3)),
            ("token", address(token).unwrap()),
            ("amount", i128v(50_000_000)),
            ("fee_bps", u32v(300)),
            ("deadline", u64v(1_795_000_000)),
            ("relayer_allowed", boolv(true)),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::{keys::testing::identity, scval::*};
    use stellar_xdr::WriteXdr;

    const TOKEN: &str = "CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC";

    #[test]
    fn decodes_funded() {
        let (_, m) = identity(1);
        let ev = decode_event(&topics("funded", Some(4)), &funded_value(&m.address(), TOKEN)).unwrap().unwrap();
        assert_eq!(
            ev,
            ChainEvent::Funded {
                id: 4,
                maintainer: m.address(),
                repo_id: 1_101_352_203,
                issue: 3,
                token: TOKEN.into(),
                amount: 50_000_000,
                fee_bps: 300,
                deadline: 1_795_000_000,
                relayer_allowed: true
            }
        );
        assert_eq!((ev.bounty_id(), ev.kind()), (4, "funded"));
    }

    #[test]
    fn decodes_released_refunded_disputed_resolved() {
        let (_, a) = identity(1);
        let (_, b) = identity(2);
        let rel = map_of(vec![
            ("winner", address(&a.address()).unwrap()),
            ("net", i128v(48_500_000)),
            ("fee", i128v(1_500_000)),
            ("caller", address(&b.address()).unwrap()),
        ]);
        assert_eq!(
            decode_event(&topics("released", Some(4)), &rel).unwrap(),
            Some(ChainEvent::Released { id: 4, winner: a.address(), net: 48_500_000, fee: 1_500_000, caller: b.address() })
        );
        let refd = map_of(vec![("amount", i128v(7))]);
        assert_eq!(decode_event(&topics("refunded", Some(5)), &refd).unwrap(), Some(ChainEvent::Refunded { id: 5, amount: 7 }));
        let disp = map_of(vec![("caller", address(&b.address()).unwrap())]);
        assert_eq!(decode_event(&topics("disputed", Some(6)), &disp).unwrap(), Some(ChainEvent::Disputed { id: 6, caller: b.address() }));

        let split = map_of(vec![("winner", ScVal::Void), ("winner_net", i128v(0)), ("fee", i128v(0)), ("maintainer_amount", i128v(100))]);
        assert_eq!(
            decode_event(&topics("resolved", Some(6)), &split).unwrap(),
            Some(ChainEvent::Resolved { id: 6, winner: None, winner_net: 0, fee: 0, maintainer_amount: 100 })
        );
        let split2 = map_of(vec![
            ("winner", address(&a.address()).unwrap()),
            ("winner_net", i128v(90)),
            ("fee", i128v(3)),
            ("maintainer_amount", i128v(7)),
        ]);
        assert!(
            matches!(decode_event(&topics("resolved", Some(6)), &split2).unwrap(), Some(ChainEvent::Resolved { winner: Some(w), .. }) if w == a.address())
        );
    }

    #[test]
    fn untracked_events_are_ignored_not_errors() {
        let empty = map_of(vec![]);
        for name in ["paused", "unpaused", "upgraded", "config", "mystery"] {
            assert_eq!(decode_event(&topics(name, None), &empty).unwrap(), None, "{name}");
        }
        // a different contract namespace
        assert_eq!(decode_event(&[symbol("transfer").unwrap(), symbol("funded").unwrap()], &empty).unwrap(), None);
        assert_eq!(decode_event(&[], &empty).unwrap(), None);
    }

    #[test]
    fn malformed_tracked_events_are_errors() {
        let (_, m) = identity(1);
        // missing id topic
        assert!(decode_event(&topics("funded", None), &funded_value(&m.address(), TOKEN)).is_err());
        // missing field
        assert!(decode_event(&topics("refunded", Some(1)), &map_of(vec![])).is_err());
        // wrong type
        assert!(decode_event(&topics("refunded", Some(1)), &map_of(vec![("amount", u64v(1))])).is_err());
        // value is not a map
        assert!(decode_event(&topics("refunded", Some(1)), &u64v(1)).is_err());
    }

    #[test]
    fn raw_events_decode_from_base64_xdr() {
        let (_, m) = identity(1);
        let b64 = |v: &ScVal| STANDARD.encode(v.to_xdr(Limits::none()).unwrap());
        let raw = RawEvent {
            id: "0000004294967296-0000000001".into(),
            ledger: 1,
            tx_hash: "ab".repeat(32),
            ledger_closed_at: "2026-11-03T10:00:00Z".into(),
            topics_b64: topics("funded", Some(9)).iter().map(b64).collect(),
            value_b64: b64(&funded_value(&m.address(), TOKEN)),
            in_successful_contract_call: true,
        };
        assert_eq!(raw.decode().unwrap().unwrap().bounty_id(), 9);
        let broken = RawEvent { value_b64: "!!!".into(), ..raw };
        assert!(broken.decode().is_err());
    }

    #[test]
    fn cursor_ledger_reads_the_high_bits() {
        assert_eq!(cursor_ledger("0000004294967296-0000000001"), 1);
        assert_eq!(cursor_ledger("0000000000000000-0000000000"), 0);
        assert_eq!(cursor_ledger("garbage"), 0);
        // ledger 2_345_678 in the high 32 bits
        assert_eq!(cursor_ledger(&format!("{:019}-0000000002", 2_345_678u64 << 32)), 2_345_678);
    }
}
