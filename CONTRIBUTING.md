# Contributing to escrow-backend

Thank you for helping improve SkillEscrow! This guide will help you get started with development and understand how to submit your changes.

## Setup

### Requirements
- Rust stable ≥ 1.88 (see `rust-toolchain.toml`)
- PostgreSQL 14+ (tested on 18)
- For tests: a Postgres role with `CREATEDB` privilege

### Initial setup
```bash
export DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres
cp .env.example .env
# Fill in BOUNTY_CONTRACT_ID and the three secrets (GITHUB_APP_PRIVATE_KEY, CSRF_SECRET, WEBHOOK_SECRET)
```

## Running checks locally

The test suite requires a running Postgres database. All tests run in parallel using throwaway databases created by `#[sqlx::test]`, so they never modify your main database.

```bash
scripts/test.sh          # fmt --check, clippy -D warnings, all tests, cargo audit
cargo test --workspace            # run all tests under a minute once built
cargo clippy --workspace --all-targets -- -D warnings  # lint checks
cargo fmt --all --check            # formatting checks
```

For TypeScript backend tests (the original `src/` implementation):
```bash
npm ci
npm run test                 # vitest suite
npm run typecheck            # TypeScript type checking
```

## Architecture rules

The dependency graph between crates is enforced by tests, not conventions. These rules are non-negotiable:

- **escrow-core**: Pure logic, no I/O. No `tokio`, `reqwest`, or `sqlx`. Tests verify this.
- **escrow-testkit**: Only a dev-dependency. Never imported by runtime code.
- **escrow-chain** and **escrow-payouts**: Cannot reach each other; both are reached only by `escrow-engine`.
- **escrow-ai**: Isolated. Cannot reach money code and is not reachable from it.

The test `crates/escrowd/tests/architecture.rs` enforces these rules. If your change violates them, the build will fail with a clear message.

**Before opening a PR, make sure architecture tests pass:**
```bash
cargo test -p escrowd architecture
```

## Code conventions

### Format and lints
- All code must pass `cargo fmt` (config: `rustfmt.toml`)
- All code must pass `cargo clippy` with `-D warnings`
- No `unsafe` code allowed

### Testing
- Test names are sentences describing the behaviour (e.g., `fails_if_amount_exceeds_max`)
- Any code that moves money or decides who may move it needs tests for both success and failure paths
- Configuration validation must happen at startup, not runtime (see `config.rs`)
- When fixing a bug: write a failing test first, then make it pass

### Naming and style
- Use explicit error types, not strings
- Friend errors that come from user action; only real errors are Infrastructure failures
- Statuses are stored as text in Postgres (with `CHECK` constraints, so typos are caught)

## Where to make changes

| To change… | Look in |
|---|---|
| Money rules, parsers, config | `crates/escrow-core` |
| Stellar/Soroban integration, event shapes, tx building | `crates/escrow-chain` |
| Database schema or queries | `crates/escrow-db/migrations/`, then one module per area |
| Payout behaviour | `crates/escrow-payouts` |
| Webhook/command/PR handling, indexer, worker | `crates/escrow-engine` |
| HTTP routes | `crates/escrow-api` (update `docs/openapi.yaml` too) |
| AI prompt or evaluation | `crates/escrow-ai`, then run `escrowd eval` |
| CLI wiring, subcommands, `doctor` | `crates/escrowd` |

## Pull request checklist

Before opening a PR:

- [ ] Code passes `cargo fmt`, `cargo clippy`, and all tests
- [ ] Architecture test passes (`cargo test -p escrowd architecture`)
- [ ] New routes are documented in `docs/openapi.yaml`
- [ ] Database changes are forward-only migrations (never edit a shipped migration)
- [ ] Bug fixes include a test demonstrating the fix (write the failing test first)
- [ ] Money-moving code has tests for both success and failure paths
- [ ] No `unsafe` code
- [ ] No `#[allow(...)]` attributes without a comment explaining why
- [ ] Commit message is clear and references the issue (e.g., "fixes #123")

## Running locally

```bash
createdb escrow
cargo run -- doctor             # check environment and contracts
cargo run -- all               # start API, indexer, and worker
```

You can develop without the GitHub App or relayer secrets—the API and indexer will work, but GitHub-dependent jobs will wait.

To test against the real testnet contract (read-only, no side effects):
```bash
DATABASE_URL=postgres://…/escrow_scratch \
GITHUB_APP_ID= GITHUB_APP_SLUG= GITHUB_APP_PRIVATE_KEY= \
GITHUB_CLIENT_ID= GITHUB_CLIENT_SECRET= RELAYER_SECRET= \
cargo run -- reindex --from-ledger <recent-ledger>
```

## Test coverage

Generate a coverage report locally:
```bash
cargo install cargo-llvm-cov
cargo llvm-cov --workspace --html
open target/llvm-cov/html/index.html
```

Coverage is also generated and uploaded to CI on each push.

## Live tests (read-only)

To test against the real Stellar testnet contract:
```bash
LIVE_ACCOUNT=G… LIVE_TOKEN=C… cargo test -p escrow-chain --test live_testnet -- --ignored --nocapture
```

The relayer path (prepares but does not send):
```bash
RELAYER_SECRET=S… LIVE_BOUNTY=4 LIVE_WINNER=G… cargo test -p escrow-chain --test live_testnet -- --ignored --nocapture relayer
```

## Questions?

See [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) for more details on specific tasks, and [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for how the system is designed.

---

This is an open-source project. By contributing, you agree that your contributions will be licensed under the same license as the project.
