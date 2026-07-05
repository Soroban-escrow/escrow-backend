# Progress

What was built, what was verified and how, and what is open. Dates are 2026; the work was done in one sitting on 4–5 November.

## Built

| Area | State |
|---|---|
| `escrow-core`: amounts, fees, `/bounty` grammar, criteria and closing-keyword parsing, card, webhook HMAC, CSRF, PKCE/OAuth state, config | done, 59 tests |
| `escrow-chain`: Soroban RPC client, XDR, tx build/simulate/assemble/sign, event decoding, submission allow-list, relayer (with archived-state restore) | done, 54 tests + 4 live |
| `escrow-db`: schema, repositories, Postgres job queue | done, 27 tests on real Postgres |
| `escrow-github`: App JWT + installation tokens, REST, OAuth, payload types | done, 23 tests |
| `escrow-payouts`: merge → payout state machine, relayer loop, crash/expiry recovery | done, 20 tests |
| `escrow-ai`: advisory reviewer, prompt fencing, ranking, 12 eval cases | done, 41 tests |
| `escrow-engine`: indexer, commands, PRs, webhooks, card sync, reminders, alerts, worker | done, 63 tests |
| `escrow-api`: 29 operations, sign-in, CSRF, wallet linking, funding, admin, webhooks, metrics, limits | done, 48 tests |
| `escrowd`: serve / worker / all / migrate / doctor / sync-installations / reindex / eval / healthcheck | done; architecture rules enforced by 6 tests |
| Docs, OpenAPI (kept honest by a test), Docker, Compose, Caddy, CI, import-from-Prisma script | done |

**341 tests, all passing. Clippy clean with warnings denied. `cargo audit` clean (345 crates). `unsafe` forbidden.**

## Verified against the real world

Against the live testnet contract (`CCEKBPYC…`) and the real GitHub App, with the real `.env` credentials, using a scratch database and with GitHub and the relayer switched off where a side effect was possible:

- `escrowd doctor`: database, RPC, contract config, the relayer matches the contract's registered relayer and its balance, GitHub App authenticated (RS256 JWT accepted by GitHub), indexer.
- Contract reads: config; bounties 1–4 with the right statuses; all eight historical lifecycle events decoded (`funded`×4, `released`×2, `disputed`, `resolved`).
- A real unsigned `fund_bounty` transaction built and simulated end to end.
- **The relayer's signing path against the real contract**: `release(relayer, 4, winner)` simulated by the contract with the relayer as source account, assembled and signed, then stopped before sending (bounty #4 left untouched). A nonexistent bounty is rejected at simulation (`BountyNotFound`) before any signing.
- The indexer on an empty database **rebuilt bounty #4 from raw chain events**, identical to the TypeScript app's row.
- The HTTP API served over a real socket from `escrowd all`; `/bounties`, `/bounties/4`, `/config`, `/stats`, `/metrics`, `/health`, 401s and the bad-signature webhook behaved as specified.
- The **unmodified Next.js web app** rendered the live bounty from the Rust API (home, detail, and the `/api` proxy).
- The Prisma import ran on a copy of the TypeScript database (see MIGRATING).
- `cargo build --release` produces an 11.7 MB binary.

## Verified against fakes only

The send/wait/resubmit/restore steps of the payout path, webhook processing, sign-in, comments and cards run against in-memory fakes (an in-process fake Soroban RPC and fake GitHub). The cryptography is real (ed25519 signatures verified, XDR round-trips, a real generated RSA key for the JWT), the protocol shapes mirror what the real services return, and where I could check them live I did. The behaviour that has not run for real with this backend is the end-to-end **merge → relayer release → comment** on testnet.

## Open

1. **Merge-to-payout on testnet with the Rust backend.** Needs a second GitHub account and a second funded testnet wallet (the user-side steps). The TypeScript backend completed this run (funded → released → no double pay) against the same contract.
2. **The Docker image has not been built here** (no Docker in the authoring environment). CI builds it on every push and runs the binary; `docker-compose.yml` and the Caddyfile are syntax-checked only.
3. **KMS-backed relayer signer** for mainnet (`ReleaseSigner` is the seam; raw secrets are refused on mainnet).
4. **Restore for user-signed transactions on archived state** (the relayer restores for itself, tested against a fake RPC; for user-signed transactions the API reports `state_archived`).
5. **Distributed rate limiting** (per process today, by choice).
6. Refund, dispute and AI-review flows have not run live (the contract's dispute path was exercised on testnet earlier by the TypeScript tooling; the AI reviewer has never called the real API because no key was available, and `escrowd eval` is ready for the first run).
7. Pre-mainnet list: audit, soak, tested restores, admin multisig (see the runbook).

## Notable decisions during the build

- JWT signing moved from the pure-Rust `rsa` crate to `aws-lc-rs` when `cargo audit` flagged RUSTSEC-2023-0071 (Marvin timing attack) and a `crypto-bigint` advisory. Test keys are generated at run time, so no private key is committed.
- `STELLAR_RPC_URL` is mandatory on mainnet instead of defaulting to a provider nobody has vetted.
- The architecture test originally forbade `ed25519-dalek` from the AI crate's graph; it only arrives through the JWT library, so the rule was narrowed to what actually matters (no `stellar-xdr`, no chain/payout crates).
- A dead `HORIZON_URL` setting was removed rather than documented.
