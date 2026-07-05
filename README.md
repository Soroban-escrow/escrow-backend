<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/img/banner-dark.svg">
    <img alt="SkillEscrow backend: the chain is the source of truth" src="docs/img/banner-light.svg" width="100%">
  </picture>
</p>

# escrow-backend

The backend of **SkillEscrow Bounties**, written in Rust. A maintainer comments `/bounty 50 USDC` on a GitHub issue, funds it from a wallet into a Soroban escrow contract, and **merging the pull request that closes the issue pays its author from escrow, in seconds**. This service is the part in the middle: it indexes the contract, listens to GitHub, keeps the bounty card on the issue current, relays payouts, and serves the API the web app talks to.

[![ci](https://github.com/Soroban-escrow/escrow-backend/actions/workflows/ci.yml/badge.svg)](https://github.com/Soroban-escrow/escrow-backend/actions/workflows/ci.yml)

> **One idea runs through the whole design: the chain is the source of truth.** The database is a cache of it, plus the GitHub-side bookkeeping a chain cannot know. Bounty status is written by exactly one component (the indexer, from contract events). Every money decision re-reads the chain. Lose the database and you re-index; corrupt a row and the next event corrects it.

| | |
|---|---|
| **Language / runtime** | Rust 2024, tokio, axum, sqlx |
| **Moving parts** | one binary (`escrowd`) + PostgreSQL. No Redis, no Node |
| **Size** | ~16k lines across 9 crates, **337 tests** (all green), clippy clean with warnings denied, `cargo audit` clean, `unsafe` forbidden; the release binary is 11.7 MB |
| **Verified against** | the real Stellar testnet contract and the real GitHub App (see [what has been proven](#what-has-and-has-not-been-proven)) |
| **Pairs with** | [`escrow-contracts`](https://github.com/Soroban-escrow/escrow-contracts) (the Soroban contract) and [`escrow-app`](https://github.com/Soroban-escrow/escrow-app) (the web UI) |

---

## Contents

- [Where this fits](#where-this-fits)
- [How it works](#how-it-works)
- [The rules the design keeps](#the-rules-the-design-keeps)
- [Crates](#crates)
- [Quick start](#quick-start)
- [`escrowd` commands](#escrowd-commands)
- [HTTP API](#http-api)
- [Testing](#testing)
- [What has and has not been proven](#what-has-and-has-not-been-proven)
- [Security](#security)
- [Deploying it](#deploying-it)
- [Two backends in one repository](#two-backends-in-one-repository)
- [Repository layout](#repository-layout)
- [Not done yet](#not-done-yet)
- [Documentation](#documentation)

---

## Where this fits

```
   maintainer / contributor                                    Stellar network
          │                                                   ┌────────────────┐
          │ comments, PRs, merges                             │ bounty_escrow  │
          ▼                                                   │ (Soroban)      │
   ┌─────────────┐   webhooks    ┌──────────────────┐  RPC    │  holds funds   │
   │   GitHub    │──────────────▶│  escrow-backend  │◀───────▶│  emits events  │
   │  (the App)  │◀──────────────│     (this repo)  │         └────────────────┘
   └─────────────┘  comments,    └──────────────────┘                  ▲
                    labels             ▲   │ REST                      │ signs with
                                       │   ▼                           │ their own wallet
                                ┌──────────────────┐            ┌──────┴───────┐
                                │    escrow-app    │───────────▶│   Freighter  │
                                │ (Next.js web UI) │            └──────────────┘
                                └──────────────────┘
```

- **[escrow-contracts](https://github.com/Soroban-escrow/escrow-contracts)** holds the money and the rules: fund, release, refund, dispute, resolve, a fee cap, a refund grace period, and an accounting invariant. It is the authority.
- **escrow-backend** (this repo) never decides *whether* someone is paid. It notices what the contract did, tells GitHub, and, when the maintainer opted in, asks the contract to release.
- **[escrow-app](https://github.com/Soroban-escrow/escrow-app)** is the web UI. It works unchanged against this backend (verified: see below).

## How it works

A bounty's life, and who does what at each step:

```
 1  /bounty 50 USDC 14d        webhook ─▶ job queue ─▶ command handler
                                           checks write access, creates a funding link, replies on the issue
 2  maintainer opens the link  API builds an UNSIGNED fund_bounty transaction (simulated against the chain);
                                their wallet signs it; the API relays it. The server never sees their key.
 3  contract emits `funded`    indexer ─▶ bounty row (OPEN) ─▶ job ─▶ card posted on the issue + label
 4  contributor opens a PR     "Closes #123" ─▶ PR linked to the bounty ─▶ (optional) AI review comment
 5  maintainer merges          webhook ─▶ payout row, one per bounty. Chain re-read: still OPEN?
                                 ├─ contributor has a linked wallet, maintainer allowed the relayer ─▶ QUEUED
                                 ├─ no wallet yet  ─▶ PENDING_WALLET (asks on the PR; resumes when linked)
                                 └─ relayer not allowed ─▶ AWAITING_MAINTAINER (they sign the release)
 6  relayer loop               signs release(), stores the tx hash BEFORE waiting, waits, confirms
 7  contract emits `released`  indexer ─▶ bounty RELEASED, payout CONFIRMED, "Paid 4.85 XLM to @dev" comment
 8  nobody merges              after deadline + 72 h grace the maintainer refunds themselves
 9  disagreement               /bounty dispute ─▶ arbiter (admin) opens and resolves on chain with a split
```

Everything GitHub sends is acknowledged immediately and processed by the worker from a **durable Postgres job queue**, so a slow GitHub or a restart never loses a delivery.

## The rules the design keeps

These are the properties the system is built around. Each is enforced in code **and** pinned by tests, so they survive refactoring.

| Rule | Where it lives | How it is pinned |
|---|---|---|
| **The chain decides.** Payout code re-reads the bounty from the chain; it never trusts the database for a money decision. | `escrow-payouts` | `a_bounty_that_changed_under_us_fails_the_payout_without_submitting`, `the_chain_says_is_closed_never_gets_a_payout` |
| **One payout per bounty.** The idempotency key `release:<id>` is unique; the first merge wins. | `payouts::create_pending` + unique index | `the_first_merged_pr_wins_and_a_second_is_told_so` |
| **Hash before wait.** The submitted transaction hash is stored *before* anyone waits on it, so a crash can never cause a second submission. | `escrow-payouts::relay_release` | `a_release_is_submitted_waited_on_and_confirmed_with_the_hash_stored_first`, `a_crash_after_submitting_resumes_by_waiting_not_by_submitting_again` |
| **Status has one writer.** Only the indexer, from contract events, changes a bounty's status. No HTTP route can. | `escrow-engine::indexer` | `status_is_only_changed_by_the_apply_functions`; replay safety: `replaying_a_page_changes_nothing_and_queues_nothing_more` |
| **The AI never touches money.** It reads PRs and writes one comment. No crate on the money path can reach it, and it cannot reach the money path. | crate dependency graph | `architecture.rs` reads `cargo metadata` and fails the build if either direction appears |
| **Keys stay with their owners.** The server prepares transactions; wallets sign. The only key it holds is the relayer's, which can do exactly one thing (`release`), only when the maintainer opted in. | `escrow-chain`, the contract | `build_fund_uses_the_next_sequence_number…` (never signs), `relayer_stops_before_signing_when_the_contract_would_reject` |
| **Untrusted text stays text.** Issue bodies, PR text and diffs are escaped before they are echoed and fenced before they reach the model. | `escrow-core::card`, `escrow-ai::input` | `hostile_criteria_text_cannot_inject_markup_into_the_card`, `untrusted_text_cannot_close_the_fence` |

## Crates

```
escrow-core      pure logic: amounts, fees, /bounty parser, criteria, webhook HMAC, OAuth state, config
   ▲              (no I/O: a test forbids tokio / reqwest / sqlx here)
   ├── escrow-chain     Soroban RPC, XDR, tx building and signing, event decoding, the relayer
   ├── escrow-db        schema migrations and typed repositories over PostgreSQL (incl. the job queue)
   ├── escrow-github    GitHub App client (JWT → installation token), OAuth calls, webhook payload types
   │       ▲
   │       └── escrow-ai       advisory PR reviewer (Anthropic), prompt fencing, ranking, eval cases
   ├── escrow-payouts   merge → payout state machine and the relayer loop       (cannot reach escrow-ai)
   ├── escrow-engine    indexer, command / PR / webhook handlers, card sync, ops, the worker  (cannot reach escrow-ai)
   ├── escrow-api       the axum HTTP API                                         (cannot reach escrow-ai)
   ├── escrow-testkit   in-memory fakes shared by tests (never a runtime dependency)
   └── escrowd          the binary: wires every crate together; the ONLY place the AI is connected
```

| Crate | Responsibility | Tests |
|---|---:|---:|
| `escrow-core` | the money rules and every parser of untrusted text | 59 |
| `escrow-chain` | everything that speaks Stellar | 35 + 15 fake-RPC + 3 live (opt-in) |
| `escrow-db` | schema and queries, verified on real Postgres | 27 |
| `escrow-github` | GitHub App auth and REST | 23 |
| `escrow-payouts` | the payout state machine | 20 |
| `escrow-ai` | the reviewer | 41 |
| `escrow-engine` | indexer, handlers, worker | 63 |
| `escrow-api` | routes, auth, CSRF, limits | 48 |
| `escrowd` | architecture guard rails | 6 |

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the data model, the queue, failure handling and the reasoning behind each decision.

## Quick start

You need **Rust (stable, ≥ 1.88)** and **PostgreSQL 14+**. Everything else is optional until you want it.

```bash
git clone https://github.com/Soroban-escrow/escrow-backend.git
cd escrow-backend
git checkout rust-backend

createdb escrow                      # or point DATABASE_URL at any empty database
cp .env.example .env                 # then edit: DATABASE_URL, BOUNTY_CONTRACT_ID, the three secrets
                                     #   openssl rand -hex 32   (once each for SESSION_SECRET, CSRF_SECRET, GITHUB_WEBHOOK_SECRET)

cargo run --release -- doctor        # checks config, database, RPC, the contract, the relayer, GitHub
cargo run --release -- all           # API + worker in one process, on :3001
```

`doctor` tells you precisely what is wrong and how to fix it. A healthy testnet setup looks like this (this is real output from the live testnet contract):

```
[  ok  ] configuration  valid (Testnet, contract CCEKBPYC…)
[  ok  ] database       connected, 1 migration(s) applied
[  ok  ] stellar rpc    reachable, ledger 5571436
[  ok  ] contract       fee 3.00%, refund grace 72 h, arbiter GAISL54R…
[  ok  ] relayer        GDMYOW4B…, balance 9999.98 XLM
[ warn ] indexer        has not run yet (no cursor). It starts with the worker.
[  ok  ] github app     authenticated; installed on 1 account(s)
[  ok  ] XLM token      CDLZFC3S…
```

Only `DATABASE_URL`, `BOUNTY_CONTRACT_ID` and the three secrets are required. Without the GitHub App the API and indexer still run (read-only), and GitHub-dependent work simply waits for the credentials. Without `RELAYER_SECRET` maintainers sign every release themselves. Without `ANTHROPIC_API_KEY` there are no AI reviews (they are optional and never affect payouts). To rebuild state from an existing deployment:

```bash
escrowd sync-installations           # pull the App's installations and repositories from GitHub
escrowd reindex --from-ledger 5521000   # replay contract events (safe to repeat)
```

Run the web app against it by pointing its `API_INTERNAL_URL` at this server; it needs no changes.

## `escrowd` commands

| Command | What it does |
|---|---|
| `escrowd serve` | The HTTP API only. |
| `escrowd worker` | Indexer, relayer loop, job queue, hourly refund reminders, per-minute alert checks, daily housekeeping. |
| `escrowd all` | Both in one process: the simple single-host deployment. |
| `escrowd doctor` | Checks configuration and every dependency, with fixes. Exits non-zero on a hard failure. |
| `escrowd migrate` | Applies database migrations and exits. (`serve`/`worker`/`all` migrate on start unless `AUTO_MIGRATE=false`.) |
| `escrowd sync-installations` | Reconciles GitHub App installations and repositories now. |
| `escrowd reindex --from-ledger N` | Replays contract events from ledger N. Idempotent. |
| `escrowd eval` | Scores the AI reviewer against 12 built-in cases, including prompt-injection attacks (needs `ANTHROPIC_API_KEY`; costs a few cents). |
| `escrowd healthcheck` | Probes the local `/health`; exit 0 when healthy. Used by the container health check (the runtime image has no curl). |

## HTTP API

29 operations, fully described in [`docs/openapi.yaml`](docs/openapi.yaml) (a test fails the build if the spec and the router disagree) and walked through with examples in [docs/API.md](docs/API.md).

| Area | Routes |
|---|---|
| Public | `GET /bounties` (filter, sort, paginate), `GET /bounties/{id}`, `GET /users/{login}`, `GET /config`, `GET /stats` |
| Sign-in | `GET /auth/login`, `GET /auth/callback`, `GET /auth/me`, `POST /auth/logout` |
| Wallet | `POST /wallet/challenge`, `POST /wallet/verify`, `DELETE /wallet` |
| Me | `GET /me/earnings`, `GET /me/funded` |
| Funding | `GET /intents/{id}`, `POST /intents/{id}/build`, `POST /tx/submit`, `POST /bounties/{id}/release/build`, `POST /bounties/{id}/refund/build` |
| Admin | `GET /admin/disputes`, `POST /admin/disputes/{id}/open-onchain`, `…/resolve/build`, `…/reject`, `GET /admin/payouts`, `POST /admin/payouts/{id}/retry`, `GET /admin/status` |
| GitHub | `POST /github/webhooks` |
| Ops | `GET /health`, `GET /metrics` (Prometheus) |

Errors are always `{ "error": { "code", "message" } }`. Anything the contract itself rejects comes back as `contract_<Name>` (for example `contract_IssueAlreadyFunded`), so a UI can explain it.

## Testing

```bash
# Postgres is needed for the database-backed tests. Each test gets its own throwaway database.
export DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres
cargo test --workspace            # 337 tests; under a minute once built
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

What the tests cover, in the order the money flows:

- **core**: amounts and fees (the same arithmetic as the contract, including overflow and rounding), the `/bounty` grammar, acceptance-criteria and closing-keyword parsing, webhook HMAC, CSRF, PKCE and the OAuth state cookie, config validation.
- **chain**: XDR round-trips, `ScVal` encoding of every contract argument, decoding of every event, transaction assembly and signing, the submission allow-list (only our contract, only user-facing functions, always signed), and the RPC client against an in-process fake RPC.
- **db**: against real Postgres: guarded status transitions, pagination under ties, `NUMERIC` amounts beyond `u64`, concurrent challenge burning and job claiming, lease recovery.
- **payouts**: crash recovery, expired-transaction handling, backoff, deterministic-vs-transient failures, a stumbling payout not blocking its neighbours.
- **engine**: indexer replay safety, quiet-contract cursors, retention resets, malformed-event handling; every command and webhook path; card posting under concurrency; the queue end to end.
- **api**: the real router, driven in-process: full sign-in flow, CSRF, real SEP-53 wallet signatures, forged webhooks, dispute resolution, per-user rate limits.
- **architecture**: the dependency rules above, enforced from `cargo metadata`.

Opt-in live tests read the real testnet contract (nothing is signed or sent):

```bash
LIVE_ACCOUNT=G… LIVE_TOKEN=C… cargo test -p escrow-chain --test live_testnet -- --ignored --nocapture
```

More in [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).

## What has and has not been proven

Honest accounting, because a payments system should have it.

**Verified against the real world (testnet contract `CCEKBPYC…`, the real GitHub App):**

- The Rust client reads the contract's configuration and bounties 1 to 4 correctly, decodes all eight historical lifecycle events (`funded`, `released`, `disputed`, `resolved`), and simulates a real unsigned `fund_bounty` transaction end to end (account lookup, argument encoding, token-transfer simulation, fee and resource assembly).
- The indexer, started on an empty database, **rebuilt bounty #4 from raw chain events** identically to the TypeScript app's database (OPEN, 5 XLM, 3% fee, relayer allowed, same funding transaction).
- The GitHub App JWT (RS256) is accepted by real GitHub; installations and repositories sync.
- The relayer key matches the relayer registered in the contract.
- The **unmodified Next.js web app** renders the live bounty from this API (home page, detail page, and the `/api` proxy).

**Verified only against fakes (but with full fidelity at the protocol level):** the rest of the payout path (relayer signing and submission, wait, resubmit-after-expiry), webhook handling, sign-in, comments and cards. The signing code is checked with real ed25519 verification and real XDR round-trips; GitHub and RPC responses are faked. **A merge-to-payout run on testnet with this backend has not been done yet**; the previous TypeScript backend did one successfully (funded → released → no double pay), and the contract is the same.

**Not built in the authoring environment:** the Docker image (no Docker there). CI builds it on every push, and `cargo build --release` of the same binary was run locally.

## Security

Summary; the full threat model is in [docs/SECURITY.md](docs/SECURITY.md).

- Webhooks: HMAC-SHA256 over the **raw** body, constant-time compare, verified before parsing; delivery ids de-duplicated; body size capped.
- Sessions: random 256-bit ids, stored **hashed**; `HttpOnly`, `SameSite=Lax`, `Secure` over https; 30-day expiry; CSRF token bound to the session on every state-changing call.
- Sign-in: OAuth with PKCE and a signed, expiring state cookie; redirect targets restricted to same-site paths (no open redirect).
- Wallet linking: a SEP-53 signature over a message binding the GitHub account, the address, a single-use nonce and an expiry; the challenge is burned atomically.
- Relaying: only signed, single-operation calls to *our* contract's user functions pass; everything else is refused before the network. `upgrade`, `set_admin`, `pause` and the other admin functions can never go through this service.
- GitHub requests: every path segment that comes from outside (repo name, login, sha, label) is validated, so a hostile name can never redirect a request to another endpoint (including `..` segments).
- Secrets never reach logs or `Debug` output; the RPC URL (which can embed an API key) is stripped from network errors; query strings (which carry one-time OAuth codes) are not logged.
- Rate limits per user (or per address), a global ceiling per address, `X-Forwarded-For` honoured only when `TRUST_PROXY` is set.
- `cargo audit` is clean, and `#![forbid(unsafe_code)]` is set in every crate.
- Mainnet refuses a raw `RELAYER_SECRET`; a KMS-backed signer is required there (see [Not done yet](#not-done-yet)).

## Deploying it

One host, three containers (Postgres, this backend, Caddy for automatic HTTPS) via [`docker-compose.yml`](docker-compose.yml); the backend is one static-ish binary, so a plain `systemd` unit works just as well. [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) walks through a **free always-on setup** (an Oracle Cloud Always Free VM for the API and worker, Vercel for the web app), because the worker has to stay awake: it runs the indexer, the relayer loop and the job queue, so hosts that put free services to sleep (Render's free tier, for example) break payouts. [docs/RUNBOOK.md](docs/RUNBOOK.md) covers day-two operations: every alert, what it means and what to do.

## Two backends in one repository

This repository also contains the **original TypeScript API** (Fastify, Prisma, BullMQ) at the repository root (`src/`, `prisma/`, `test/`, `package.json`, `api/`, `vercel.json`), exactly as it was. It is kept as the reference implementation and for the existing Vercel deployment.

| | TypeScript (`src/`) | Rust (`crates/`) |
|---|---|---|
| HTTP API | the contract the web app uses | **the same routes and JSON shapes** (the web app works unchanged) |
| Queue | BullMQ on Redis | Postgres (`FOR UPDATE SKIP LOCKED`, leases, backoff) |
| Processes | API, worker, Redis, Postgres | `escrowd`, Postgres |
| Environment | `.env` | **the same variable names** (so an existing `.env` works; `REDIS_URL` is simply ignored) |
| Database | Prisma schema | its own schema; rebuilt by `reindex` + `sync-installations`, or imported (see below) |

Moving over, what differs, and a one-shot data import from a Prisma database: [docs/MIGRATING.md](docs/MIGRATING.md).

## Repository layout

```
Cargo.toml  Cargo.lock  rust-toolchain.toml  rustfmt.toml
crates/
  escrow-core/      pure logic            escrow-payouts/   payout state machine
  escrow-chain/     Stellar               escrow-ai/        advisory reviewer
  escrow-db/        Postgres + migrations escrow-engine/    indexer, handlers, worker
  escrow-github/    GitHub App client     escrow-api/       axum API
  escrow-testkit/   test fakes            escrowd/          the binary
docs/               ARCHITECTURE  API  openapi.yaml  CONFIGURATION  DEPLOYMENT  RUNBOOK  SECURITY
                    DEVELOPMENT  MIGRATING  PROGRESS      img/
deploy/Caddyfile    Dockerfile  docker-compose.yml  .env.example
scripts/            test.sh  import-from-prisma.sql
.github/            ci.yml (fmt, clippy, tests, audit, image build, secret scan)  dependabot.yml
src/ prisma/ test/ api/ eval/ package.json …   the original TypeScript API (reference)
```

## Not done yet

Stated plainly, in rough order of importance before real money:

1. **A merge-to-payout run on testnet with this Rust backend.** Everything up to the wire is tested and the indexer/read paths are verified live; the signing-and-submit path has only run against a fake RPC. It needs the user-side steps (a second GitHub account and funded testnet wallet).
2. **A KMS-backed relayer signer for mainnet.** `ReleaseSigner` is the seam; the config already refuses `RELAYER_SECRET` on mainnet.
3. **Restoring archived contract state.** If the RPC reports archived entries the API returns a clear `state_archived` error; it does not yet build the restore transaction.
4. **A distributed rate limiter.** Limits are per process (deliberately: no Redis). Fine for one instance.
5. **External audit, a staging soak, tested backup restores**, and the usual pre-mainnet list in [docs/RUNBOOK.md](docs/RUNBOOK.md).

## Documentation

| | |
|---|---|
| [ARCHITECTURE](docs/ARCHITECTURE.md) | components, data model, the queue, failure handling, design decisions |
| [API](docs/API.md) · [openapi.yaml](docs/openapi.yaml) | every route, with examples |
| [CONFIGURATION](docs/CONFIGURATION.md) | every environment variable |
| [DEPLOYMENT](docs/DEPLOYMENT.md) | a free always-on setup, plus the generic version |
| [RUNBOOK](docs/RUNBOOK.md) | operating it: alerts, recovery, backups, rotating secrets, the mainnet checklist |
| [SECURITY](docs/SECURITY.md) | threat model and known limits |
| [DEVELOPMENT](docs/DEVELOPMENT.md) | building, testing, adding a route or an event |
| [MIGRATING](docs/MIGRATING.md) | from the TypeScript backend |
| [PROGRESS](docs/PROGRESS.md) | what was built and verified, and the open items |
