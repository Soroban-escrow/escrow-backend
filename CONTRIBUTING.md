# Contributing to SkillEscrow Backend

Thank you for contributing to SkillEscrow! We welcome pull requests, bug fixes, documentation improvements, and architectural reviews.

Before getting started, please review this guide. It outlines our development setup, architecture guardrails, quality checks, and pull request requirements.

For deep-dive technical documentation on layout, recipes (adding routes, migrations, events, injection tests), and live test guidelines, see [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).

---

## Core Philosophy

1. **The chain is the source of truth.** The database is an ephemeral cache of on-chain state plus GitHub-side metadata. Bounty status is authored strictly by the indexer from contract events. Every monetary release or validation step re-verifies against the Soroban contract.
2. **Fail-closed security.** `#![forbid(unsafe_code)]` is strictly enforced across all crates. Money rules must be pure and deterministic.
3. **Clean separation of concerns.** Money logic never touches AI evaluation, and AI components never build or sign Stellar transactions.

---

## Development Setup

### Prerequisites

- **Rust toolchain**: Rust stable ≥ 1.88 (`rustfmt` and `clippy` components required; see `rust-toolchain.toml`).
- **PostgreSQL**: PostgreSQL 14+ (tested on 18).
- **PostgreSQL Permissions**: A user role with `CREATEDB` privileges. `#[sqlx::test]` dynamically provisions and drops isolated throwaway databases per test, enabling fearless parallel test execution.

### Environment

Copy `.env.example` to `.env` and set `DATABASE_URL`:

```bash
cp .env.example .env
export DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres
```

Run pre-flight checks:

```bash
cargo run -- doctor
```

---

## Running Quality Checks

All pull requests must pass the full verification pipeline before merge.

### Automated Test Suite

Run the full check suite via the `Makefile` or test script:

```bash
make check
# Equivalent to: ./scripts/test.sh
```

This runs:
1. `cargo fmt --all --check`
2. `cargo clippy --workspace --all-targets --locked -- -D warnings`
3. `cargo test --workspace --locked`
4. `cargo audit` (if installed)

### TypeScript Reference Suite

If modifying TypeScript or root packaging:

```bash
npm ci
npm run test
npm run typecheck
```

---

## Architecture Guardrails

Our architectural boundaries are strictly asserted by automated tests in [`crates/escrowd/tests/architecture.rs`](crates/escrowd/tests/architecture.rs). If an architecture test fails, refactor your code to adhere to these rules rather than relaxing the test:

1. **Nothing on the money path can reach the AI reviewer**:
   `escrow-core`, `escrow-chain`, `escrow-db`, `escrow-payouts`, `escrow-engine`, `escrow-api`, and `escrow-github` must never transitively depend on `escrow-ai`. An advisory AI review must never influence a payout decision.
2. **The AI reviewer cannot reach anything that moves money**:
   `escrow-ai` cannot reach `escrow-payouts`, `escrow-chain`, `escrow-engine`, `escrow-api`, `stellar-xdr`, or `stellar-strkey`. It must only read PR diffs and produce advisory suggestions.
3. **The core crate is pure**:
   `escrow-core` must stay completely free of I/O (no `tokio`, `reqwest`, `sqlx`, `hyper`, `axum`, etc.) so money logic and parsing can be tested purely in memory.
4. **Only the binary links everything**:
   Libraries never depend on `escrowd`. Only the top-level binary wires together the API, worker, engine, and AI reviewer.
5. **The testkit never ships**:
   `escrow-testkit` is strictly for `[dev-dependencies]` and test mocks; production binaries must never link it.
6. **The database layer is network-independent**:
   `escrow-db` does not depend on network crates (`reqwest`, `escrow-chain`, `escrow-github`, `axum`).

---

## Pull Request Checklist

Before opening a pull request, ensure:

- [ ] Code is formatted with `cargo fmt --all`.
- [ ] Clippy passes with zero warnings (`cargo clippy --workspace --all-targets --locked -- -D warnings`).
- [ ] All tests pass (`cargo test --workspace --locked`).
- [ ] Architecture guardrail tests pass (`cargo test -p escrowd --test architecture`).
- [ ] Any database migration is forward-only, additive, and placed in `crates/escrow-db/migrations/000N_name.sql` with `CHECK` constraints on status fields.
- [ ] If modifying API endpoints, update `docs/openapi.yaml` and verify that documentation tests in `escrow-api` pass.
- [ ] Any user-facing or configuration change is documented in `docs/CONFIGURATION.md` or `docs/API.md`.
- [ ] Commit messages follow standard Conventional Commits conventions (e.g. `feat:`, `fix:`, `refactor:`, `docs:`, `test:`).
- [ ] No credentials, secret keys, or `.env` files are committed.
