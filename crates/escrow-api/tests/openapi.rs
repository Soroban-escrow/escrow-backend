//! The OpenAPI document and the router must describe the same routes. Adding a route without documenting it
//! (or documenting one that does not exist) fails here.

use std::collections::BTreeSet;

fn documented() -> BTreeSet<String> {
    let spec = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/openapi.yaml")).expect("docs/openapi.yaml exists");
    let (mut out, mut path, mut in_paths) = (BTreeSet::new(), None::<String>, false);
    for line in spec.lines() {
        if line == "paths:" {
            in_paths = true;
        } else if in_paths && !line.starts_with(' ') && !line.is_empty() {
            in_paths = false;
        }
        if !in_paths {
            continue;
        }
        if let Some(p) = line.strip_prefix("  /").and_then(|l| l.strip_suffix(':')) {
            path = Some(format!("/{p}"));
        } else if let (Some(p), Some(m)) = (&path, line.strip_prefix("    ").and_then(|l| l.strip_suffix(':')))
            && ["get", "post", "delete", "put", "patch"].contains(&m)
        {
            out.insert(format!("{} {p}", m.to_uppercase()));
        }
    }
    out
}

fn routed() -> BTreeSet<String> {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs")).unwrap();
    let mut out = BTreeSet::new();
    for line in src.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(".route(\"")
            && let Some((path, tail)) = rest.split_once("\", ")
        {
            let method = tail.split('(').next().unwrap().to_uppercase();
            out.insert(format!("{method} {path}"));
        }
    }
    out
}

#[test]
fn every_route_is_documented_and_every_documented_route_exists() {
    let (docs, code) = (documented(), routed());
    assert!(code.len() >= 25, "the route scan found too little: {code:?}");
    let undocumented: Vec<_> = code.difference(&docs).collect();
    let phantom: Vec<_> = docs.difference(&code).collect();
    assert!(undocumented.is_empty(), "routes missing from docs/openapi.yaml: {undocumented:?}");
    assert!(phantom.is_empty(), "docs/openapi.yaml documents routes that do not exist: {phantom:?}");
}

#[test]
fn the_spec_is_internally_consistent() {
    let spec = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/openapi.yaml")).unwrap();
    // every $ref points at something defined
    for part in spec.split("$ref: '#/").skip(1) {
        let target = part.split('\'').next().unwrap();
        let (kind, name) = target.rsplit_once('/').unwrap();
        let section = kind.rsplit('/').next().unwrap();
        let needle = format!("\n    {name}:");
        let block_start = spec.find(&format!("\n  {section}:")).unwrap_or_else(|| panic!("missing components section {section}"));
        assert!(spec[block_start..].contains(&needle), "dangling $ref: #/{target}");
    }
}
