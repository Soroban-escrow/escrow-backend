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
They read the real contract's config and bounties, decode every real event in the RPC's retention window, and simulate (never sign or send) a real `fund_bounty`. `LIVE_ACCOUNT` must be a funded testnet account.

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

## Conventions

- `cargo fmt` (config in `rustfmt.toml`), clippy clean with `-D warnings`, no `unsafe`.
- Test names are sentences describing the behaviour. A bug fix starts with a failing test.
- Anything that moves money, or decides who may, needs a test for the failure path as well as the success path.
- Errors that a user can cause get a friendly reply; only infrastructure failures are errors. A job that can never succeed is buried, not retried.
