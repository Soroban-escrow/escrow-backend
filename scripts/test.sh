#!/usr/bin/env bash
# Runs the whole quality gate the way CI does.
#   scripts/test.sh                         uses $DATABASE_URL, or postgres://localhost/postgres
#   DATABASE_URL=postgres://u:p@host/db scripts/test.sh
#
# Database-backed tests use #[sqlx::test]: each test creates and drops its own throwaway database, so the role in
# DATABASE_URL needs CREATEDB. Nothing in the database it points at is read or modified.
set -euo pipefail
cd "$(dirname "$0")/.."
export DATABASE_URL="${DATABASE_URL:-postgres://localhost/postgres}"
echo "== fmt";    cargo fmt --all --check
echo "== clippy"; cargo clippy --workspace --all-targets --locked -- -D warnings
echo "== test";   cargo test --workspace --locked
if command -v cargo-audit >/dev/null 2>&1; then echo "== audit"; cargo audit; else echo "== audit skipped (cargo install cargo-audit)"; fi
echo "all green"
