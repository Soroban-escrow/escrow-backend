#![no_main]

use base64::{Engine, engine::general_purpose::STANDARD};
use escrow_chain::{
    events::decode_scval,
    scval::{
        Fields, read_address, read_bool, read_enum_variant, read_i128, read_opt_address, read_symbol, read_u32, read_u64,
    },
};
use libfuzzer_sys::fuzz_target;
use stellar_xdr::{Limits, ReadXdr, ScVal};

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = decode_scval(s);
    }

    let b64 = STANDARD.encode(data);
    let _ = decode_scval(&b64);

    if let Ok(val) = ScVal::from_xdr(data, Limits { depth: 64, len: 1 << 20 }) {
        let _ = read_u64(&val);
        let _ = read_u32(&val);
        let _ = read_bool(&val);
        let _ = read_i128(&val);
        let _ = read_address(&val);
        let _ = read_opt_address(&val);
        let _ = read_symbol(&val);
        let _ = read_enum_variant(&val);
        if let Ok(fields) = Fields::of(&val) {
            for key in ["id", "maintainer", "winner", "caller", "amount", "fee_bps", "relayer_allowed", "status"] {
                let _ = fields.get(key);
            }
        }
    }
});
