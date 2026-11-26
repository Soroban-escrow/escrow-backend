# Development

## Setup

- Rust stable ≥ 1.88 (`rust-toolchain.toml` selects stable with rustfmt and clippy)
- PostgreSQL 14+ (tested on 18)
- For the tests: a Postgres role with `CREATEDB`

```bash
export DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres
scripts/test.sh          # fmt --check, clippy -D warnings, all tests, cargo audit (if installed)
```

`#[sqlx::test]` creates and drops a throwaway database per test, so the tests run in parallel and never read or modify the database `DATABASE_URL` points at. Nothing needs a network except the three opt-in live tests.

## TypeScript backend tests

The original TypeScript backend (`src/`, `test/`, `package.json`) runs in CI alongside the Rust backend:

```bash
npm ci                           # install dependencies
npm run test                     # run vitest suite
npm run typecheck                # TypeScript type checking
```

The TypeScript backend continues to support the Vercel deployment. While the Rust backend is the primary implementation, the TypeScript tests ensure the reference implementation remains working.

## Test coverage (Rust)

To generate a coverage report locally:

```bash
cargo install cargo-llvm-cov
export DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres
cargo llvm-cov --workspace --html
open target/llvm-cov/html/index.html
```

The coverage report shows which branches in the payout and indexer paths are exercised by tests. Coverage is also reported in CI and uploaded as an artifact on each push.

## Running locally

```bash
createdb escrow && cp .env.example .env     # fill in DATABASE_URL, BOUNTY_CONTRACT_ID and the three secrets
cargo run -- doctor
cargo run -- all
```
You do not need the GitHub App or a relayer to develop: without them the API, indexer and read endpoints work, and GitHub-dependent jobs wait.

To try it against the real testnet contract without any side effects, run with the GitHub variables and `RELAYER_SECRET` blank (blank means unset), so nothing can comment on a real issue or send a payment, and a scratch database:

```bash
DATABASE_URL=postgres://…/escrow_scratch GITHUB_APP_ID= GITHUB_APP_SLUG= GITHUB_APP_PRIVATE_KEY= \
GITHUB_CLIENT_ID= GITHUB_CLIENT_SECRET= RELAYER_SECRET= cargo run -- reindex --from-ledger <recent>
```

## Live tests (read-only)

```bash
LIVE_ACCOUNT=G… LIVE_TOKEN=C… cargo test -p escrow-chain --test live_testnet -- --ignored --nocapture
```
They read the real contract's config and bounties, decode every real event in the RPC's retention window, and simulate (never send) a real `fund_bounty`. `LIVE_ACCOUNT` must be a funded testnet account.

A fourth live test runs the relayer's whole preparation path (simulate `release` against the real contract, assemble, sign) and **stops before sending**, so no funds move:
```bash
RELAYER_SECRET=S… LIVE_BOUNTY=4 LIVE_WINNER=G… cargo test -p escrow-chain --test live_testnet -- --ignored --nocapture relayer
```
`LIVE_WINNER` must be an existing funded account that is not the bounty's maintainer; the bounty must be open and funded with relayer permission.

## Layout and where things go

| To change… | Look in |
|---|---|
| a money rule, a parser, config | `escrow-core` (no I/O allowed; a test enforces it) |
| how we talk to Stellar, an event shape, tx building | `escrow-chain` |
| the schema or a query | `escrow-db` (`migrations/`, one module per area) |
| payout behaviour | `escrow-payouts` |
| a webhook / command / PR behaviour, the indexer, the worker | `escrow-engine` |
| a route | `escrow-api` (and `docs/openapi.yaml`) |
| the AI prompt, schema or cases | `escrow-ai` (then run `escrowd eval`) |
| wiring, subcommands, `doctor` | `escrowd` |

## Recipes

**Add a route.** Write the handler in `escrow-api/src/routes/`, register it in `lib.rs` with `.route("/path", get(handler))` on one line (the documentation test scans for that form), and add it to `docs/openapi.yaml`. `openapi.rs` fails until the two agree.

**Add a migration.** Add `crates/escrow-db/migrations/000N_name.sql`. Migrations are forward-only and additive; never edit a migration that has shipped. Constrain new status columns with `CHECK`.

**Handle a new contract event.** Add a variant to `ChainEvent` and its decoding in `escrow-chain/src/events.rs` (with a test that builds the event as XDR); apply it in `escrow-engine/src/indexer.rs::apply_event` (idempotent, and decide what jobs it enqueues). A *tracked* event that fails to decode stops the indexer on purpose.

**Add a GitHub command.** Extend `escrow-core::command` (pure parser, with tests), then handle it in `escrow-engine/src/commands.rs`. Remember: first line of the comment only.

**Add a fake behaviour for a test.** `escrow-testkit` holds the shared in-memory `FakeGithub`, `FakeChain`, `FakeEvents` and `FakeTxBuilder`. It is a dev-dependency only (a test enforces that).

**Add a prompt injection test case.** The offline injection corpus is in `crates/escrow-ai/tests/injection.rs`. To add a case:
1. Add a new entry to the `cases` vec with format: `("case_descriptive_name", payload, "Human description")`.
2. The payload is any string that might bypass the `defang()` function or fence structure.
3. The test automatically verifies that:
   - Fences remain balanced (exactly one open and one close of each fence type)
   - Payload size stays within limits
   - Defang removes all fence-like patterns
4. Examples of injection techniques tested: fence-closing variants (case/whitespace), zero-width characters, homoglyphs, encoding (base64/rot13), very long inputs, schema imitation, nested structures, unicode normalization. At least 40 cases must exist and pass for CI to succeed.
5. Run `cargo test -p escrow-ai injection` to verify.

## Fuzzing

The event and `ScVal` decoders have `cargo-fuzz` targets under `crates/escrow-chain/fuzz`:

```bash
cargo install cargo-fuzz
cargo +nightly fuzz run --fuzz-dir crates/escrow-chain/fuzz decode_event
cargo +nightly fuzz run --fuzz-dir crates/escrow-chain/fuzz decode_scval
```

## Contract errors

`CONTRACT_ERRORS` in `crates/escrow-core/src/contract_errors.rs` mirrors the contract's `errors.rs` by hand. To catch drift, that file is vendored in `crates/escrow-core/tests/fixtures/` at a pinned commit of `Soroban-escrow/escrow-contracts` (`bounty_escrow_errors.commit`), and the `contract_errors_sync` test compares codes and names. On a mismatch the test (and so CI) fails with a diff of missing, renamed and extra codes.

When the contract changes its errors:

```bash
scripts/sync-contract-errors.sh [<commit-sha>]   # default: head of the contracts repo; needs `gh`
# edit CONTRACT_ERRORS (code, name, friendly message) until this passes:
cargo test -p escrow-core --test contract_errors_sync
```

Commit the updated fixture files together with the table change. The test needs no network.

>>>>>>> 7461cc4 (ci(contract-errors): check the error table against a pinned escrow-contracts errors.rs (Closes #195))
## Conventions

- `cargo fmt` (config in `rustfmt.toml`), clippy clean with `-D warnings`, no `unsafe`.
- Test names are sentences describing the behaviour. A bug fix starts with a failing test.
- Anything that moves money, or decides who may, needs a test for the failure path as well as the success path.
- Errors that a user can cause get a friendly reply; only infrastructure failures are errors. A job that can never succeed is buried, not retried.
