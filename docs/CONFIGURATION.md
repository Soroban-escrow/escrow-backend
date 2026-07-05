# Configuration

All configuration is environment variables, validated **at start-up**: a bad deploy fails immediately with every problem listed at once (try it: run `escrowd doctor` with an empty environment). A `.env` file in the working directory is loaded automatically (`ENV_FILE=/path` to use another); real environment variables always win over the file. The names match the TypeScript API, so an existing `.env` works unchanged (`REDIS_URL` is simply ignored).

Secrets never appear in logs or `Debug` output.

## Required

| Variable | Notes |
|---|---|
| `DATABASE_URL` | PostgreSQL URL. Prisma-style `?schema=public` suffixes are fine. |
| `BOUNTY_CONTRACT_ID` | The deployed escrow contract, `C…` (56 chars). |
| `SESSION_SECRET` | ≥ 32 chars. Signs the OAuth state cookie. `openssl rand -hex 32`. |
| `CSRF_SECRET` | ≥ 32 chars. Binds CSRF tokens to sessions. |
| `GITHUB_WEBHOOK_SECRET` | ≥ 8 chars. Must equal the secret set on the GitHub App. |

## Server and database

| Variable | Default | Notes |
|---|---|---|
| `PORT` | `3001` | |
| `LOG_LEVEL` | `info` | Any `tracing` filter, e.g. `info,escrow_engine=debug`. `RUST_LOG` takes precedence. |
| `LOG_FORMAT` | pretty | `json` for log shippers. The container image defaults to `json`. |
| `DB_MAX_CONNECTIONS` | `10` | |
| `AUTO_MIGRATE` | `true` | Apply migrations when `serve`/`worker`/`all` start. Set `false` to run `escrowd migrate` yourself. |
| `API_PUBLIC_URL` | `http://localhost:3001` | Where browsers and GitHub reach the API. **Must be `https://` in production**: it decides whether cookies are `Secure`, and is the OAuth callback base (`{API_PUBLIC_URL}/auth/callback`). |
| `WEB_PUBLIC_URL` | `http://localhost:3000` | The web app. The only CORS origin; where sign-in returns to; used in links the bot posts. |
| `TRUST_PROXY` | off | `1`/`true` only behind a reverse proxy you control: then `X-Forwarded-For` identifies callers for rate limiting. Left off, the header is ignored, so it cannot be used to dodge limits. |
| `METRICS_TOKEN` | none | When set, `/metrics` needs `Authorization: Bearer <token>`. |

## Stellar

| Variable | Default | Notes |
|---|---|---|
| `STELLAR_NETWORK` | `testnet` | `testnet`, `mainnet` or `local`. Sets the passphrase and explorer defaults. |
| `STELLAR_RPC_URL` | testnet: `https://soroban-testnet.stellar.org` | **Required on mainnet**: choose a provider on purpose. |
| `STELLAR_NETWORK_PASSPHRASE` | by network | Override only for `local`/custom networks. |
| `TOKEN_XLM_CONTRACT_ID`, `TOKEN_USDC_CONTRACT_ID` | empty | The token contracts bounties can be paid in. **Leave one empty to disable that token** (`/bounty … USDC` is then refused with a clear message). On testnet, the native XLM token's contract id is the Stellar Asset Contract for native; USDC needs a verified issuer's contract. |
| `TOKEN_MIN_AMOUNT`, `TOKEN_MAX_AMOUNT` | `1`, `500` | Per-bounty limits **in whole tokens**, enforced when parsing `/bounty`. The contract enforces its own (authoritative) limits too. |
| `MIN_DAYS`, `MAX_DAYS` | `1`, `90` | Allowed bounty duration. |
| `INDEXER_START_LEDGER` | none | Where a brand-new database starts reading events. Best: the contract's deployment ledger. |
| `INDEXER_LOOKBACK_LEDGERS` | `1000` | If no start ledger: begin this many ledgers behind the tip. |
| `INDEXER_INTERVAL_SECS` | `5` | Poll interval. |
| `EXPLORER_TX_URL_TEMPLATE` | by network | Must contain `{hash}`. Used in "Paid …" comments. |

## GitHub App (all five, or none)

`GITHUB_APP_ID`, `GITHUB_APP_SLUG`, `GITHUB_APP_PRIVATE_KEY` (the PEM; literal `\n` accepted), `GITHUB_CLIENT_ID`, `GITHUB_CLIENT_SECRET`. Setting some but not all is a start-up error. Registration steps are in the app repo's `docs/GITHUB_APP.md`. Without these the API, indexer and read endpoints run; sign-in answers `501`, and GitHub-dependent jobs wait (retrying with backoff) until the App is configured.

| Variable | Notes |
|---|---|
| `ADMIN_GITHUB_IDS` | Comma-separated numeric GitHub user ids allowed to use the dispute/admin routes. Re-applied at every login, so removing an id revokes access at that person's next sign-in. |

## Relayer (automatic payouts)

| Variable | Default | Notes |
|---|---|---|
| `RELAYER_SECRET` | none | A funded `S…` secret key whose address is registered in the contract as the relayer. **Refused on mainnet** (start-up error): use a KMS-backed signer there. At worker start the address is compared with the contract's registered relayer; a mismatch disables automatic payouts and logs an error rather than burning attempts. |
| `RELAYER_MIN_XLM_ALERT` | `50` | Alert when the relayer's balance drops below this. |
| `ALERT_WEBHOOK_URL` | none | Receives `{ "text": "SkillEscrow: …" }` for each alert (Slack-compatible), each distinct message at most once an hour. |

## AI review (optional, advisory)

Needs the GitHub App **and** a key; otherwise reviews are off. It never affects payouts.

| Variable | Default | Notes |
|---|---|---|
| `ANTHROPIC_API_KEY` | none | |
| `AI_MODEL` | `claude-sonnet-5-5` | |
| `AI_MAX_REVIEWS_PER_PR_PER_DAY` | `5` | Cost control. |
| `AI_MAX_REVIEWS_PER_REPO_PER_DAY` | `50` | Cost control. |
| `REVIEW_DEBOUNCE_SECS` | `300` | Quiet period after a push before reviewing, so a burst costs one review. |

## Worker

| Variable | Default | Notes |
|---|---|---|
| `WORKER_CONCURRENCY` | `4` | Jobs processed in parallel. |

## Mainnet differences, in one place

`STELLAR_NETWORK=mainnet` makes `STELLAR_RPC_URL` mandatory, switches the passphrase and explorer defaults, refuses `RELAYER_SECRET`, and `doctor` fails (not warns) if `API_PUBLIC_URL` is not `https://`.
