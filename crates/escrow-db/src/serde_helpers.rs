//! JSON compatibility with the web app: 64-bit ids travel as strings (JavaScript numbers lose precision).

use serde::Serializer;

pub fn i64_string<S: Serializer>(v: &i64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&v.to_string())
}

pub fn opt_i64_string<S: Serializer>(v: &Option<i64>, s: S) -> Result<S::Ok, S::Error> {
    match v {
        Some(v) => s.serialize_str(&v.to_string()),
        None => s.serialize_none(),
    }
}

#[cfg(test)]
mod tests {
    use serde::Serialize;

    #[derive(Serialize)]
    struct T {
        #[serde(serialize_with = "super::i64_string")]
        a: i64,
        #[serde(serialize_with = "super::opt_i64_string")]
        b: Option<i64>,
        #[serde(serialize_with = "super::opt_i64_string")]
        c: Option<i64>,
    }

    #[test]
    fn large_ids_survive_javascript() {
        let j = serde_json::to_string(&T { a: 9_007_199_254_740_993, b: Some(7), c: None }).unwrap();
        assert_eq!(j, r#"{"a":"9007199254740993","b":"7","c":null}"#);
    }
}
