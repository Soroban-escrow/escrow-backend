//! The chain client against a fake Soroban RPC. No network: the server is an in-process axum app
//! that records every request, so the tests can assert both what we send and how we react.

use axum::{Json, Router, extract::State, routing::post};
use base64::{Engine, engine::general_purpose::STANDARD};
use escrow_chain::{
    ChainError, ChainReader, ChainSettings, EventSource, FundParams, Relayer, ReleaseSigner, SorobanChain, TxBuilder, TxOutcome,
    keys::Signer,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use stellar_xdr::*;

const CONTRACT: &str = "CCEKBPYCWKNTXIQLXJHVAF7RYGUZZ6MITA7VVJBOK5BHXUO44ELZ4F4V";
const TOKEN: &str = "CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC";
const PASS: &str = "Test SDF Network ; September 2015";

type Log = Arc<Mutex<Vec<(String, Value)>>>;
type Responder = Arc<dyn Fn(&str, &Value) -> Value + Send + Sync>;

struct Fake {
    url: String,
    log: Log,
}

impl Fake {
    async fn start(responder: impl Fn(&str, &Value) -> Value + Send + Sync + 'static) -> Fake {
        let log: Log = Arc::default();
        let state = (log.clone(), Arc::new(responder) as Responder);
        async fn handle(State((log, r)): State<(Log, Responder)>, Json(req): Json<Value>) -> Json<Value> {
            let method = req["method"].as_str().unwrap_or("").to_string();
            let params = req["params"].clone();
            let out = r(&method, &params);
            log.lock().unwrap().push((method, params));
            Json(match out.get("__error") {
                Some(e) => json!({ "jsonrpc": "2.0", "id": 1, "error": e }),
                None => json!({ "jsonrpc": "2.0", "id": 1, "result": out }),
            })
        }
        let app = Router::new().route("/", post(handle)).with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Fake { url, log }
    }

    fn chain(&self) -> SorobanChain {
        SorobanChain::new(&ChainSettings::new(&self.url, PASS, CONTRACT)).unwrap()
    }

    fn calls(&self, method: &str) -> Vec<Value> {
        self.log.lock().unwrap().iter().filter(|(m, _)| m == method).map(|(_, p)| p.clone()).collect()
    }
}

fn b64<T: WriteXdr>(v: &T) -> String {
    v.to_xdr_base64(Limits::none()).unwrap()
}

fn signer(seed: u8) -> (String, Signer) {
    let secret = stellar_strkey::Unredacted(stellar_strkey::ed25519::PrivateKey([seed; 32])).to_string();
    let s = Signer::from_secret(&secret).unwrap();
    (secret, s)
}

fn account_entry(address: &str, seq: i64, balance: i64) -> Value {
    let pk = stellar_strkey::ed25519::PublicKey::from_string(address).unwrap().0;
    let data = LedgerEntryData::Account(AccountEntry {
        account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(pk))),
        balance,
        seq_num: SequenceNumber(seq),
        ..Default::default()
    });
    json!({ "entries": [{ "xdr": b64(&data) }], "latestLedger": 100 })
}

fn sim_ok(result: Option<&ScVal>) -> Value {
    json!({
        "transactionData": b64(&SorobanTransactionData::default()),
        "minResourceFee": "50000",
        "results": [{ "auth": [], "xdr": result.map(b64) }],
        "latestLedger": 100
    })
}

fn sym(s: &str) -> ScVal {
    ScVal::Symbol(ScSymbol::try_from(s).unwrap())
}
fn addr(a: &str) -> ScVal {
    ScVal::Address(a.parse().unwrap())
}
fn map(pairs: Vec<(&str, ScVal)>) -> ScVal {
    ScVal::Map(Some(ScMap::sorted_from(pairs.into_iter().map(|(k, v)| ScMapEntry { key: sym(k), val: v }).collect::<Vec<_>>()).unwrap()))
}
fn i128v(v: i128) -> ScVal {
    ScVal::I128(Int128Parts { hi: (v >> 64) as i64, lo: v as u64 })
}

fn bounty_value(maintainer: &str, status: &str, winner: Option<&str>) -> ScVal {
    map(vec![
        ("id", ScVal::U64(4)),
        ("maintainer", addr(maintainer)),
        ("repo_id", ScVal::U64(1_101_352_203)),
        ("issue", ScVal::U64(3)),
        ("token", addr(TOKEN)),
        ("amount", i128v(50_000_000)),
        ("fee_bps", ScVal::U32(300)),
        ("deadline", ScVal::U64(1_795_000_000)),
        ("relayer_allowed", ScVal::Bool(true)),
        ("status", ScVal::Vec(Some(ScVec(vec![sym(status)].try_into().unwrap())))),
        ("winner", winner.map_or(ScVal::Void, addr)),
        ("created_at", ScVal::U64(1_794_000_000)),
    ])
}

fn decode_envelope(x: &str) -> TransactionV1Envelope {
    match TransactionEnvelope::from_xdr_base64(x, Limits::none()).unwrap() {
        TransactionEnvelope::Tx(e) => e,
        _ => panic!("expected a V1 envelope"),
    }
}

fn invoked(e: &TransactionV1Envelope) -> &InvokeContractArgs {
    match &e.tx.operations[0].body {
        OperationBody::InvokeHostFunction(InvokeHostFunctionOp { host_function: HostFunction::InvokeContract(c), .. }) => c,
        _ => panic!("not a contract call"),
    }
}

#[tokio::test]
async fn reads_a_bounty_from_a_read_only_simulation() {
    let (_, m) = signer(1);
    let (_, w) = signer(2);
    let value = bounty_value(&m.address(), "Released", Some(&w.address()));
    let fake = Fake::start(move |method, _| match method {
        "simulateTransaction" => sim_ok(Some(&value)),
        other => panic!("unexpected {other}"),
    })
    .await;
    let b = fake.chain().get_bounty(4).await.unwrap();
    assert_eq!((b.id, b.amount, b.fee_bps), (4, 50_000_000, 300));
    assert_eq!(b.status, escrow_core::BountyStatus::Released);
    assert_eq!(b.winner.as_deref(), Some(w.address().as_str()));
    // the read used the throwaway zero account, so it needs no funded key
    let sent = fake.calls("simulateTransaction")[0]["transaction"].as_str().unwrap().to_string();
    let e = decode_envelope(&sent);
    assert_eq!(e.tx.source_account, MuxedAccount::Ed25519(Uint256([0; 32])));
    assert_eq!(invoked(&e).function_name.to_utf8_string_lossy(), "get_bounty");
}

#[tokio::test]
async fn a_missing_bounty_surfaces_the_contract_error() {
    let fake = Fake::start(|_, _| json!({ "error": "HostError: Error(Contract, #10)" })).await;
    let e = fake.chain().get_bounty(99).await.unwrap_err();
    assert_eq!(e.contract_error(), Some("BountyNotFound"));
}

#[tokio::test]
async fn build_fund_uses_the_next_sequence_number_and_all_seven_arguments() {
    let (_, m) = signer(1);
    let maintainer = m.address();
    let acct = account_entry(&maintainer, 100, 900_000_000);
    let fake = Fake::start(move |method, _| match method {
        "getLedgerEntries" => acct.clone(),
        "simulateTransaction" => sim_ok(Some(&ScVal::U64(4))),
        other => panic!("unexpected {other}"),
    })
    .await;
    let xdr = fake
        .chain()
        .build_fund(&FundParams {
            maintainer: maintainer.clone(),
            repo_id: 1,
            issue: 3,
            token: TOKEN.into(),
            amount: 50_000_000,
            deadline: 1_795_000_000,
            relayer_allowed: true,
        })
        .await
        .unwrap();
    let e = decode_envelope(&xdr);
    assert!(e.signatures.is_empty(), "the server must never sign a user's transaction");
    assert_eq!(e.tx.seq_num.0, 101);
    assert_eq!(e.tx.fee, 100 + 50_000 + 10_000);
    assert!(matches!(e.tx.ext, TransactionExt::V1(_)));
    let call = invoked(&e);
    assert_eq!(call.function_name.to_utf8_string_lossy(), "fund_bounty");
    assert_eq!(call.args.len(), 7);
    assert_eq!(call.args[4], i128v(50_000_000));
    assert_eq!(call.args[6], ScVal::Bool(true));
}

#[tokio::test]
async fn building_for_an_unfunded_account_says_so() {
    let (_, m) = signer(1);
    let fake = Fake::start(|method, _| match method {
        "getLedgerEntries" => json!({ "entries": null, "latestLedger": 1 }),
        other => panic!("unexpected {other}"),
    })
    .await;
    let e = fake.chain().build_refund(&m.address(), 4).await.unwrap_err();
    assert!(matches!(e, ChainError::Invalid(ref m) if m.contains("does not exist")), "{e}");
}

#[tokio::test]
async fn resolve_encodes_a_missing_winner_as_void() {
    let (_, a) = signer(5);
    let acct = account_entry(&a.address(), 7, 1);
    let fake = Fake::start(move |method, _| match method {
        "getLedgerEntries" => acct.clone(),
        _ => sim_ok(None),
    })
    .await;
    let xdr = fake.chain().build_resolve(&a.address(), 4, None, 0).await.unwrap();
    let call_args = decode_envelope(&xdr);
    let args = &invoked(&call_args).args;
    assert_eq!((args[2].clone(), args[3].clone()), (ScVal::Void, ScVal::U32(0)));
}

#[tokio::test]
async fn submit_refuses_foreign_transactions_without_touching_the_network() {
    let (_, s) = signer(3);
    let fake = Fake::start(|_, _| panic!("the network must not be contacted")).await;
    // an unsigned envelope
    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256([3; 32])),
        fee: 100,
        seq_num: SequenceNumber(1),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![Operation { source_account: None, body: OperationBody::Inflation }].try_into().unwrap(),
        ext: TransactionExt::V0,
    };
    let env = TransactionEnvelope::Tx(TransactionV1Envelope { tx, signatures: VecM::default() });
    let err = fake.chain().submit_signed(&b64(&env), Some(&s.address())).await.unwrap_err();
    assert!(matches!(err, ChainError::Invalid(_)));
    assert!(fake.calls("sendTransaction").is_empty());
}

#[tokio::test]
async fn events_page_by_ledger_and_by_cursor() {
    let fake = Fake::start(|_, params| {
        let cursor = params["pagination"]["cursor"].as_str().unwrap_or("start").to_string();
        json!({
            "latestLedger": 500,
            "cursor": format!("next-after-{cursor}"),
            "events": [{ "id": "0000000000000000001-0000000001", "ledger": 7, "ledgerClosedAt": "2026-11-03T10:00:00Z", "txHash": "ab", "topic": [], "value": "AAAAAQ==", "inSuccessfulContractCall": true }]
        })
    })
    .await;
    let c = fake.chain();
    let first = c.events_from_ledger(123, 200).await.unwrap();
    assert_eq!(first.events.len(), 1);
    assert_eq!(first.latest_ledger, 500);
    let sent = &fake.calls("getEvents")[0];
    assert_eq!(sent["startLedger"], 123);
    assert_eq!(sent["filters"][0]["contractIds"][0], CONTRACT);
    assert!(sent["pagination"].get("cursor").is_none());

    let next = c.events_from_cursor(&first.cursor, 200).await.unwrap();
    assert_eq!(next.cursor, "next-after-next-after-start");
    let sent = &fake.calls("getEvents")[1];
    assert!(sent.get("startLedger").is_none(), "startLedger and cursor are mutually exclusive");
}

#[tokio::test]
async fn an_out_of_retention_cursor_is_recognised() {
    let fake =
        Fake::start(|_, _| json!({ "__error": { "code": -32602, "message": "startLedger must be within the ledger range: 100 - 200" } }))
            .await;
    let e = fake.chain().events_from_ledger(1, 10).await.unwrap_err();
    assert!(e.is_out_of_retention() && !e.is_transient());
}

#[tokio::test]
async fn relayer_signs_and_submits_a_release_and_returns_the_real_hash() {
    let (secret, r) = signer(9);
    let (_, w) = signer(2);
    let acct = account_entry(&r.address(), 50, 1_000_000_000);
    let fake = Fake::start(move |method, _| match method {
        "getLedgerEntries" => acct.clone(),
        "simulateTransaction" => sim_ok(Some(&ScVal::Void)),
        "sendTransaction" => json!({ "status": "PENDING", "hash": "ignored-by-client" }),
        other => panic!("unexpected {other}"),
    })
    .await;
    let relayer = Relayer::new(fake.chain(), &secret).unwrap();
    assert_eq!(relayer.address(), r.address());
    let hash = relayer.submit_release(4, &w.address()).await.unwrap();

    let sent = fake.calls("sendTransaction")[0]["transaction"].as_str().unwrap().to_string();
    let env = TransactionEnvelope::from_xdr_base64(&sent, Limits::none()).unwrap();
    // the returned hash is computed from what was actually sent, not trusted from the server's reply
    assert_eq!(hash, hex::encode(env.hash(escrow_chain::tx::network_id(PASS)).unwrap()));
    let e = decode_envelope(&sent);
    assert_eq!(e.tx.seq_num.0, 51);
    assert_eq!(e.signatures.len(), 1);
    assert!(escrow_chain::keys::verify(&r.address(), &hex::decode(&hash).unwrap(), e.signatures[0].signature.0.as_slice()));
    let call = invoked(&e);
    assert_eq!(call.function_name.to_utf8_string_lossy(), "release");
    assert_eq!(call.args[0], addr(&r.address()));
    assert_eq!(call.args[1], ScVal::U64(4));
    assert_eq!(call.args[2], addr(&w.address()));
    // the transaction expires quickly, so an unlucky one cannot linger and land later
    match &e.tx.cond {
        Preconditions::Time(t) => assert!(t.max_time.0 <= escrow_chain::tx::now_secs() + 130),
        _ => panic!("missing time bounds"),
    }
}

#[tokio::test]
async fn relayer_stops_before_signing_when_the_contract_would_reject() {
    let (secret, r) = signer(9);
    let (_, w) = signer(2);
    let acct = account_entry(&r.address(), 50, 1);
    let fake = Fake::start(move |method, _| match method {
        "getLedgerEntries" => acct.clone(),
        "simulateTransaction" => json!({ "error": "HostError: Error(Contract, #17)" }),
        other => panic!("unexpected {other}"),
    })
    .await;
    let e = Relayer::new(fake.chain(), &secret).unwrap().submit_release(4, &w.address()).await.unwrap_err();
    assert_eq!(e.contract_error(), Some("RelayerNotAllowed"));
    assert!(fake.calls("sendTransaction").is_empty());
}

#[tokio::test]
async fn send_statuses_map_to_the_right_errors() {
    let (secret, r) = signer(9);
    let (_, w) = signer(2);
    for (status, want) in [("TRY_AGAIN_LATER", "busy"), ("ERROR", "rejected"), ("DUPLICATE", "ok")] {
        let acct = account_entry(&r.address(), 1, 1);
        let fake = Fake::start(move |method, _| match method {
            "getLedgerEntries" => acct.clone(),
            "simulateTransaction" => sim_ok(None),
            _ => json!({ "status": status, "hash": "h" }),
        })
        .await;
        let res = Relayer::new(fake.chain(), &secret).unwrap().submit_release(1, &w.address()).await;
        match want {
            "ok" => assert!(res.is_ok(), "DUPLICATE means already submitted"),
            "busy" => assert!(matches!(res, Err(ChainError::Busy)) && res.unwrap_err().is_transient()),
            _ => assert!(matches!(res, Err(ChainError::Rejected(_)))),
        }
    }
}

#[tokio::test]
async fn waiting_polls_until_the_transaction_is_final() {
    let n = Arc::new(Mutex::new(0));
    let n2 = n.clone();
    let fake = Fake::start(move |_, _| {
        let mut c = n2.lock().unwrap();
        *c += 1;
        json!({ "status": if *c < 3 { "NOT_FOUND" } else { "SUCCESS" } })
    })
    .await;
    let (secret, _) = signer(9);
    let r = Relayer::new(fake.chain(), &secret).unwrap();
    assert_eq!(r.wait_for_tx("abc").await.unwrap(), TxOutcome::Success);
    assert_eq!(fake.calls("getTransaction").len(), 3);
}

#[tokio::test]
async fn the_balance_is_reported_in_xlm() {
    let (secret, r) = signer(9);
    let acct = account_entry(&r.address(), 1, 1_234_500_000);
    let fake = Fake::start(move |_, _| acct.clone()).await;
    let bal = Relayer::new(fake.chain(), &secret).unwrap().balance_xlm().await.unwrap();
    assert!((bal - 123.45).abs() < 1e-9);
}

#[tokio::test]
async fn an_unreachable_rpc_is_a_transient_error_that_does_not_leak_the_url() {
    let c = SorobanChain::new(&ChainSettings::new("http://127.0.0.1:1/secret-api-key", PASS, CONTRACT)).unwrap();
    let e = c.latest_ledger().await.unwrap_err();
    assert!(e.is_transient());
    assert!(!e.to_string().contains("secret-api-key"));
}

#[test]
fn base64_helper_is_standard() {
    assert_eq!(STANDARD.encode([0u8, 1, 2]), "AAEC");
}
