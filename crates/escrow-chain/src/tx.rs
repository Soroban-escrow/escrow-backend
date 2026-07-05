//! Transaction construction, assembly after simulation, signing, and inspection of what users submit.

use crate::{error::ChainError, keys, rpc::SimulationOk};
use sha2::{Digest, Sha256};
use stellar_xdr::{
    DecoratedSignature, HostFunction, InvokeContractArgs, InvokeHostFunctionOp, Limits, Memo, MuxedAccount, Operation, OperationBody,
    Preconditions, ReadXdr, ScSymbol, ScVal, SequenceNumber, Signature, SignatureHint, TimeBounds, TimePoint, Transaction,
    TransactionEnvelope, TransactionExt, TransactionV1Envelope, Uint256, VecM, WriteXdr,
};

pub const BASE_FEE: u32 = 100;
/// Functions of the escrow contract that a user wallet may submit through us.
pub const SUBMITTABLE_FUNCTIONS: [&str; 5] = ["fund_bounty", "release", "refund", "open_dispute", "resolve"];

pub fn network_id(passphrase: &str) -> [u8; 32] {
    Sha256::digest(passphrase.as_bytes()).into()
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// An unsigned, not-yet-simulated contract call.
pub fn invoke_tx(
    source: [u8; 32],
    seq: i64,
    contract: [u8; 32],
    function: &str,
    args: Vec<ScVal>,
    max_time: u64,
) -> Result<Transaction, ChainError> {
    let invoke = InvokeContractArgs {
        contract_address: stellar_xdr::ScAddress::Contract(stellar_xdr::ContractId(stellar_xdr::Hash(contract))),
        function_name: ScSymbol::try_from(function).map_err(|_| ChainError::Invalid(format!("bad function name {function}")))?,
        args: args.try_into().map_err(|_| ChainError::Invalid("too many arguments".into()))?,
    };
    let op = Operation {
        source_account: None,
        body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            host_function: HostFunction::InvokeContract(invoke),
            auth: VecM::default(),
        }),
    };
    Ok(Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(source)),
        fee: BASE_FEE,
        seq_num: SequenceNumber(seq),
        cond: Preconditions::Time(TimeBounds { min_time: TimePoint(0), max_time: TimePoint(max_time) }),
        memo: Memo::None,
        operations: vec![op].try_into().map_err(|_| ChainError::Invalid("operations".into()))?,
        ext: TransactionExt::V0,
    })
}

pub fn envelope(tx: Transaction) -> TransactionEnvelope {
    TransactionEnvelope::Tx(TransactionV1Envelope { tx, signatures: VecM::default() })
}

pub fn to_b64(env: &TransactionEnvelope) -> Result<String, ChainError> {
    Ok(env.to_xdr_base64(Limits::none())?)
}

pub fn from_b64(s: &str) -> Result<TransactionEnvelope, ChainError> {
    Ok(TransactionEnvelope::from_xdr_base64(s, Limits::none())?)
}

/// Applies a successful simulation: the Soroban resource data, the resource fee and the auth entries.
/// The fee bid carries a 20% margin on the resource fee; unused fee is refunded by the network.
pub fn assemble(mut tx: Transaction, sim: &SimulationOk) -> Result<Transaction, ChainError> {
    tx.ext = TransactionExt::V1(sim.transaction_data.clone());
    let resource = sim.min_resource_fee.max(0);
    let bid = i64::from(BASE_FEE).saturating_add(resource).saturating_add(resource / 5);
    tx.fee = u32::try_from(bid).unwrap_or(u32::MAX);
    let mut ops: Vec<Operation> = tx.operations.to_vec();
    if let Some(Operation { body: OperationBody::InvokeHostFunction(op), .. }) = ops.first_mut() {
        op.auth = sim.auth.clone().try_into().map_err(|_| ChainError::Decode("too many auth entries".into()))?;
    }
    tx.operations = ops.try_into().map_err(|_| ChainError::Invalid("operations".into()))?;
    Ok(tx)
}

pub fn tx_hash(env: &TransactionEnvelope, network_id: [u8; 32]) -> Result<[u8; 32], ChainError> {
    Ok(env.hash(network_id)?)
}

/// Signs the transaction with the relayer's key. Returns the signed envelope and its hash.
pub fn sign(tx: Transaction, network_id: [u8; 32], signer: &keys::Signer) -> Result<(TransactionEnvelope, [u8; 32]), ChainError> {
    let mut env = envelope(tx);
    let hash = tx_hash(&env, network_id)?;
    let sig = signer.sign(&hash);
    if let TransactionEnvelope::Tx(e) = &mut env {
        e.signatures = vec![DecoratedSignature {
            hint: SignatureHint(signer.hint()),
            signature: Signature(sig.to_vec().try_into().map_err(|_| ChainError::Invalid("signature length".into()))?),
        }]
        .try_into()
        .map_err(|_| ChainError::Invalid("signatures".into()))?;
    }
    Ok((env, hash))
}

#[derive(Debug, PartialEq, Eq)]
pub struct Inspected {
    pub hash: String,
    pub source: String,
    pub function: String,
}

/// Checks a transaction a user wants us to relay. We only forward a signed, single-operation call
/// to our own contract's user-facing functions. Anything else is refused before it reaches the network.
pub fn inspect_submission(
    xdr_b64: &str,
    network_id: [u8; 32],
    contract: &[u8; 32],
    expected_source: Option<&str>,
) -> Result<Inspected, ChainError> {
    let env = from_b64(xdr_b64).map_err(|_| ChainError::Invalid("not a valid transaction".into()))?;
    let TransactionEnvelope::Tx(e) = &env else {
        return Err(ChainError::Invalid("expected a standard transaction (no fee bumps)".into()));
    };
    if e.signatures.is_empty() {
        return Err(ChainError::Invalid("the transaction is not signed".into()));
    }
    let [op] = e.tx.operations.as_slice() else {
        return Err(ChainError::Invalid("expected a single-operation transaction".into()));
    };
    if op.source_account.is_some() {
        return Err(ChainError::Invalid("operations must use the transaction's source account".into()));
    }
    let OperationBody::InvokeHostFunction(h) = &op.body else {
        return Err(ChainError::Invalid("not a contract call".into()));
    };
    let HostFunction::InvokeContract(call) = &h.host_function else {
        return Err(ChainError::Invalid("not a contract call".into()));
    };
    match &call.contract_address {
        stellar_xdr::ScAddress::Contract(stellar_xdr::ContractId(stellar_xdr::Hash(c))) if c == contract => {}
        _ => return Err(ChainError::Invalid("this is not a call to the SkillEscrow contract".into())),
    }
    let function = call.function_name.to_utf8_string_lossy();
    if !SUBMITTABLE_FUNCTIONS.contains(&function.as_str()) {
        return Err(ChainError::Invalid(format!("function `{function}` cannot be submitted through this service")));
    }
    let MuxedAccount::Ed25519(Uint256(src)) = &e.tx.source_account else {
        return Err(ChainError::Invalid("muxed source accounts are not supported".into()));
    };
    let source = keys::account_strkey(*src);
    if expected_source.is_some_and(|x| x != source) {
        return Err(ChainError::Invalid("unexpected source account".into()));
    }
    Ok(Inspected { hash: hex::encode(tx_hash(&env, network_id)?), source, function })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{keys::testing::identity, scval};
    use stellar_xdr::SorobanTransactionData;

    const CONTRACT: &str = "CCEKBPYCWKNTXIQLXJHVAF7RYGUZZ6MITA7VVJBOK5BHXUO44ELZ4F4V";
    const OTHER: &str = "CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC";
    const PASS: &str = "Test SDF Network ; September 2015";

    fn contract() -> [u8; 32] {
        keys::contract_bytes(CONTRACT).unwrap()
    }

    fn sim(fee: i64) -> SimulationOk {
        SimulationOk { transaction_data: SorobanTransactionData::default(), min_resource_fee: fee, auth: vec![], result: None }
    }

    fn call(signer: &keys::Signer, function: &str, target: [u8; 32]) -> Transaction {
        invoke_tx(signer.public_bytes(), 41, target, function, vec![scval::u64v(4)], 1_800_000_000).unwrap()
    }

    #[test]
    fn network_id_is_sha256_of_the_passphrase() {
        // well-known testnet network id
        assert_eq!(hex::encode(network_id(PASS)), "cee0302d59844d32bdca915c8203dd44b33fbb7edc19051ea37abedf28ecd472");
    }

    #[test]
    fn assembly_sets_resources_and_a_fee_with_margin() {
        let (_, s) = identity(1);
        let tx = assemble(call(&s, "release", contract()), &sim(10_000)).unwrap();
        assert_eq!(tx.fee, 100 + 10_000 + 2_000);
        assert!(matches!(tx.ext, TransactionExt::V1(_)));
        assert_eq!(tx.seq_num.0, 41);
        // a negative or absurd estimate can never wrap the fee
        assert_eq!(assemble(call(&s, "release", contract()), &sim(-5)).unwrap().fee, 100);
        assert_eq!(assemble(call(&s, "release", contract()), &sim(i64::MAX)).unwrap().fee, u32::MAX);
    }

    #[test]
    fn signing_produces_a_verifiable_signature() {
        let (_, s) = identity(2);
        let (env, hash) = sign(call(&s, "release", contract()), network_id(PASS), &s).unwrap();
        let TransactionEnvelope::Tx(e) = &env else { panic!("expected V1") };
        assert_eq!(e.signatures.len(), 1);
        assert_eq!(e.signatures[0].hint.0, s.hint());
        assert!(keys::verify(&s.address(), &hash, e.signatures[0].signature.0.as_slice()));
        // the hash depends on the network, so a testnet signature is useless on mainnet
        assert_ne!(hash, tx_hash(&env, network_id("Public Global Stellar Network ; September 2015")).unwrap());
    }

    #[test]
    fn envelopes_round_trip_through_base64() {
        let (_, s) = identity(2);
        let env = envelope(call(&s, "refund", contract()));
        assert_eq!(from_b64(&to_b64(&env).unwrap()).unwrap(), env);
        assert!(from_b64("not xdr").is_err());
    }

    fn signed(function: &str, target: [u8; 32], who: &keys::Signer) -> String {
        to_b64(&sign(call(who, function, target), network_id(PASS), who).unwrap().0).unwrap()
    }

    #[test]
    fn accepts_a_signed_call_to_our_contract() {
        let (_, s) = identity(3);
        for f in SUBMITTABLE_FUNCTIONS {
            let i = inspect_submission(&signed(f, contract(), &s), network_id(PASS), &contract(), Some(&s.address())).unwrap();
            assert_eq!((i.source.as_str(), i.function.as_str()), (s.address().as_str(), f));
            assert_eq!(i.hash.len(), 64);
        }
    }

    #[test]
    fn refuses_calls_to_other_contracts_and_admin_functions() {
        let (_, s) = identity(3);
        let other = keys::contract_bytes(OTHER).unwrap();
        let e = inspect_submission(&signed("release", other, &s), network_id(PASS), &contract(), None).unwrap_err();
        assert!(e.to_string().contains("not a call to the SkillEscrow contract"));
        for f in ["upgrade", "set_admin", "pause", "set_fee", "init"] {
            let e = inspect_submission(&signed(f, contract(), &s), network_id(PASS), &contract(), None).unwrap_err();
            assert!(e.to_string().contains("cannot be submitted"), "{f}");
        }
    }

    #[test]
    fn refuses_unsigned_wrong_source_and_malformed() {
        let (_, s) = identity(3);
        let (_, other) = identity(4);
        let unsigned = to_b64(&envelope(call(&s, "release", contract()))).unwrap();
        assert!(inspect_submission(&unsigned, network_id(PASS), &contract(), None).unwrap_err().to_string().contains("not signed"));
        let wrong = inspect_submission(&signed("release", contract(), &s), network_id(PASS), &contract(), Some(&other.address()));
        assert!(wrong.unwrap_err().to_string().contains("unexpected source"));
        assert!(inspect_submission("AAAA", network_id(PASS), &contract(), None).is_err());
    }

    #[test]
    fn refuses_multi_operation_transactions() {
        let (_, s) = identity(3);
        let mut tx = call(&s, "release", contract());
        let op = tx.operations[0].clone();
        tx.operations = vec![op.clone(), op].try_into().unwrap();
        let b64 = to_b64(&sign(tx, network_id(PASS), &s).unwrap().0).unwrap();
        assert!(inspect_submission(&b64, network_id(PASS), &contract(), None).unwrap_err().to_string().contains("single-operation"));
    }
}
