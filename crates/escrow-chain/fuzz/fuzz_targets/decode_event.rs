#![no_main]

use base64::{Engine, engine::general_purpose::STANDARD};
use escrow_chain::events::{RawEvent, decode_event};
use escrow_chain::scval::{symbol, u64v};
use libfuzzer_sys::fuzz_target;
use stellar_xdr::{Limits, ReadXdr, ScVal};

fuzz_target!(|data: &[u8]| {
    let b64 = STANDARD.encode(data);
    let raw = RawEvent {
        id: String::new(),
        ledger: 0,
        tx_hash: String::new(),
        ledger_closed_at: String::new(),
        topics_b64: vec![b64.clone()],
        value_b64: b64,
        in_successful_contract_call: true,
    };
    let _ = raw.decode();

    if let Ok(val) = ScVal::from_xdr(data, Limits { depth: 64, len: 1 << 20 }) {
        let _ = decode_event(std::slice::from_ref(&val), &val);
        if let (Ok(ns), Ok(ev_funded), Ok(ev_resolved)) = (symbol("bounty"), symbol("funded"), symbol("resolved")) {
            let _ = decode_event(&[ns.clone(), ev_funded, u64v(1)], &val);
            let _ = decode_event(&[ns, ev_resolved, u64v(1)], &val);
        }
    }
});
