//! Building and reading Soroban values (`ScVal`).

use crate::{error::ChainError, keys};
use stellar_xdr::{Int128Parts, ScMap, ScMapEntry, ScSymbol, ScVal, ScVec};

pub fn address(s: &str) -> Result<ScVal, ChainError> {
    Ok(ScVal::Address(keys::sc_address(s)?))
}

/// `Option<Address>` as the contract ABI encodes it: the value, or `Void` for `None`.
pub fn opt_address(s: Option<&str>) -> Result<ScVal, ChainError> {
    s.map_or(Ok(ScVal::Void), address)
}

pub fn u64v(v: u64) -> ScVal {
    ScVal::U64(v)
}

pub fn u32v(v: u32) -> ScVal {
    ScVal::U32(v)
}

pub fn boolv(v: bool) -> ScVal {
    ScVal::Bool(v)
}

pub fn i128v(v: i128) -> ScVal {
    ScVal::I128(Int128Parts { hi: (v >> 64) as i64, lo: v as u64 })
}

pub fn symbol(s: &str) -> Result<ScVal, ChainError> {
    Ok(ScVal::Symbol(ScSymbol::try_from(s).map_err(|_| ChainError::Invalid(format!("bad symbol {s}")))?))
}

pub fn read_u64(v: &ScVal) -> Result<u64, ChainError> {
    match v {
        ScVal::U64(x) => Ok(*x),
        other => Err(bad("u64", other)),
    }
}

pub fn read_u32(v: &ScVal) -> Result<u32, ChainError> {
    match v {
        ScVal::U32(x) => Ok(*x),
        other => Err(bad("u32", other)),
    }
}

pub fn read_bool(v: &ScVal) -> Result<bool, ChainError> {
    match v {
        ScVal::Bool(x) => Ok(*x),
        other => Err(bad("bool", other)),
    }
}

pub fn read_i128(v: &ScVal) -> Result<i128, ChainError> {
    match v {
        ScVal::I128(p) => Ok((i128::from(p.hi) << 64) | i128::from(p.lo)),
        other => Err(bad("i128", other)),
    }
}

pub fn read_address(v: &ScVal) -> Result<String, ChainError> {
    match v {
        ScVal::Address(a) => keys::address_string(a),
        other => Err(bad("address", other)),
    }
}

pub fn read_opt_address(v: &ScVal) -> Result<Option<String>, ChainError> {
    match v {
        ScVal::Void => Ok(None),
        other => read_address(other).map(Some),
    }
}

pub fn read_symbol(v: &ScVal) -> Result<String, ChainError> {
    match v {
        ScVal::Symbol(s) => Ok(s.to_utf8_string_lossy()),
        other => Err(bad("symbol", other)),
    }
}

/// A unit-variant `#[contracttype]` enum is encoded as `Vec[Symbol(variant)]`.
pub fn read_enum_variant(v: &ScVal) -> Result<String, ChainError> {
    match v {
        ScVal::Vec(Some(ScVec(items))) if items.len() == 1 => read_symbol(&items[0]),
        other => Err(bad("enum variant", other)),
    }
}

/// A `#[contracttype]` struct or `#[contractevent]` payload is a map keyed by field-name symbols.
pub struct Fields<'a>(&'a [ScMapEntry]);

impl<'a> Fields<'a> {
    pub fn of(v: &'a ScVal) -> Result<Self, ChainError> {
        match v {
            ScVal::Map(Some(ScMap(entries))) => Ok(Self(entries.as_slice())),
            other => Err(bad("map", other)),
        }
    }

    pub fn get(&self, name: &str) -> Result<&'a ScVal, ChainError> {
        self.0
            .iter()
            .find(|e| matches!(&e.key, ScVal::Symbol(s) if s.to_utf8_string_lossy() == name))
            .map(|e| &e.val)
            .ok_or_else(|| ChainError::Decode(format!("missing field `{name}`")))
    }

    pub fn u64(&self, n: &str) -> Result<u64, ChainError> {
        self.get(n).and_then(read_u64).map_err(|e| field(n, e))
    }
    pub fn u32(&self, n: &str) -> Result<u32, ChainError> {
        self.get(n).and_then(read_u32).map_err(|e| field(n, e))
    }
    pub fn i128(&self, n: &str) -> Result<i128, ChainError> {
        self.get(n).and_then(read_i128).map_err(|e| field(n, e))
    }
    pub fn bool(&self, n: &str) -> Result<bool, ChainError> {
        self.get(n).and_then(read_bool).map_err(|e| field(n, e))
    }
    pub fn address(&self, n: &str) -> Result<String, ChainError> {
        self.get(n).and_then(read_address).map_err(|e| field(n, e))
    }
    pub fn opt_address(&self, n: &str) -> Result<Option<String>, ChainError> {
        self.get(n).and_then(read_opt_address).map_err(|e| field(n, e))
    }
    pub fn variant(&self, n: &str) -> Result<String, ChainError> {
        self.get(n).and_then(read_enum_variant).map_err(|e| field(n, e))
    }
}

fn bad(want: &str, got: &ScVal) -> ChainError {
    ChainError::Decode(format!("expected {want}, found {}", got.name()))
}

fn field(name: &str, e: ChainError) -> ChainError {
    ChainError::Decode(format!("field `{name}`: {e}"))
}

/// Test helper: builds a sorted map value from `(name, value)` pairs.
#[cfg(test)]
pub(crate) fn map_of(pairs: Vec<(&str, ScVal)>) -> ScVal {
    let entries: Vec<ScMapEntry> = pairs.into_iter().map(|(k, v)| ScMapEntry { key: symbol(k).unwrap(), val: v }).collect();
    ScVal::Map(Some(ScMap::sorted_from(entries).unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::testing::identity;

    #[test]
    fn i128_round_trips_across_the_range() {
        for v in [0i128, 1, -1, 5_000_000_000, -5_000_000_000, i128::MAX, i128::MIN, 1 << 64, -(1 << 64), (1 << 64) - 1] {
            assert_eq!(read_i128(&i128v(v)).unwrap(), v, "{v}");
        }
    }

    #[test]
    fn scalars_round_trip() {
        assert_eq!(read_u64(&u64v(u64::MAX)).unwrap(), u64::MAX);
        assert_eq!(read_u32(&u32v(300)).unwrap(), 300);
        assert!(read_bool(&boolv(true)).unwrap());
        assert!(read_u64(&u32v(1)).is_err());
        assert!(read_bool(&ScVal::Void).is_err());
    }

    #[test]
    fn options_use_void_for_none() {
        let (_, s) = identity(1);
        assert_eq!(read_opt_address(&opt_address(None).unwrap()).unwrap(), None);
        assert_eq!(read_opt_address(&opt_address(Some(&s.address())).unwrap()).unwrap(), Some(s.address()));
    }

    #[test]
    fn enum_variants_decode() {
        let v = ScVal::Vec(Some(ScVec(vec![symbol("Disputed").unwrap()].try_into().unwrap())));
        assert_eq!(read_enum_variant(&v).unwrap(), "Disputed");
        assert!(read_enum_variant(&symbol("Disputed").unwrap()).is_err());
    }

    #[test]
    fn fields_report_which_one_is_wrong() {
        let m = map_of(vec![("id", u64v(4)), ("flag", u32v(1))]);
        let f = Fields::of(&m).unwrap();
        assert_eq!(f.u64("id").unwrap(), 4);
        assert!(f.get("nope").unwrap_err().to_string().contains("missing field `nope`"));
        assert!(f.bool("flag").unwrap_err().to_string().contains("field `flag`"));
        assert!(Fields::of(&u64v(1)).is_err());
    }
}
