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
| `GITHUB_WEBHOOK_SECRET` | ≥ 8 chars (≥ 32 recommended). Must equal the secret set on the GitHub App. **Deprecation:** Secrets shorter than 32 characters produce a warning and will become a hard error in a future release. Generate with `openssl rand -hex 32`. |

## Server and database

| Variable | Default | Notes |
|---|---|---|
| `PORT` | `3001` | |
| `LOG_LEVEL` | `info` | Any `tracing` filter, e.g. `info,escrow_engine=debug`. `RUST_LOG` takes precedence. |
| `LOG_FORMAT` | pretty | `json` for log shippers. The container image defaults to `json`. |
| `DB_MAX_CONNECTIONS` | `10` | Maximum number of connections in the pool. **Recommendation:** Set to at least `5 + (WORKER_CONCURRENCY × 2)` to avoid starvation under load. `doctor` warns when below the recommended minimum. |
| `AUTO_MIGRATE` | `true` | Apply migrations when `serve`/`worker`/`all` start. Set `false` to run `escrowd migrate` yourself. |
| `API_PUBLIC_URL` | `http://localhost:3001` | Where browsers and GitHub reach the API. **Must be `https://` on mainnet** (enforced at start-up): it decides whether cookies are `Secure`, and is the OAuth callback base (`{API_PUBLIC_URL}/auth/callback`). Testnet and local accept `http://`. |
| `WEB_PUBLIC_URL` | `http://localhost:3000` | The web app. The only CORS origin; where sign-in returns to; used in links the bot posts. **Must be `https://` on mainnet** (enforced at start-up). Testnet and local accept `http://`. |
| `TRUST_PROXY` | off | `1`/`true` only behind a reverse proxy you control: then `X-Forwarded-For` identifies callers for rate limiting. Left off, the header is ignored, so it cannot be used to dodge limits. |
| `TRUST_PROXY_HOPS` | `1` | Number of trusted proxy hops to skip from the right of `X-Forwarded-For` when `TRUST_PROXY` is enabled (`1` when a single proxy like Caddy sits in front, `2` behind CDN + Caddy, etc.), preventing clients from spoofing the left-most entry. |
| `METRICS_TOKEN` | none (public) | When set, `/metrics` needs `Authorization: Bearer <token>`. Left unset, `/metrics` is public to anyone who can reach the API and exposes bounty, payout and job counts; `doctor` warns. **Required on mainnet** (start-up error). |

## Stellar

| Variable | Default | Notes |
|---|---|---|
| `STELLAR_NETWORK` | `testnet` | `testnet`, `mainnet` or `local`. Sets the passphrase and explorer defaults. |
| `STELLAR_RPC_URL` | testnet: `https://soroban-testnet.stellar.org` | **Required on mainnet**: choose a provider on purpose. |
| `STELLAR_NETWORK_PASSPHRASE` | by network | Override only for `local`/custom networks. |
| `STELLAR_RPC_TIMEOUT_SECS` | `20` | How long a single Soroban RPC call may take. Raise for slower self-hosted or paid providers; lower to fail fast in dev. Must be `>= 1`. |
| `TOKEN_XLM_CONTRACT_ID`, `TOKEN_USDC_CONTRACT_ID` | empty | The token contracts bounties can be paid in. **Leave one empty to disable that token** (a non-empty value must be a `C…` 56-character contract id, or startup fails) (`/bounty … USDC` is then refused with a clear message). On testnet, the native XLM token's contract id is the Stellar Asset Contract for native; USDC needs a verified issuer's contract. |
| `TOKEN_MIN_AMOUNT`, `TOKEN_MAX_AMOUNT` | `1`, `500` | Per-bounty limits **in whole tokens**, enforced when parsing `/bounty`. The contract enforces its own (authoritative) limits too. |
| `MIN_DAYS`, `MAX_DAYS` | `1`, `90` | Allowed bounty duration. |
| `INDEXER_START_LEDGER` | none | Where a brand-new database starts reading events. Best: the contract's deployment ledger. Must be a whole number (a ledger sequence, `0`–`4294967295`); anything else, such as `12o45`, is a start-up error rather than being ignored. |
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
| `RELAYER_CONFIRM_WAIT_SECS` | `60` | How long the relayer waits for a submitted release to confirm, in seconds. Must be `1`..=`120` (the submit-validity window). Raise on congested networks; tests use a few milliseconds. |
| `ALERT_WEBHOOK_URL` | none | Receives `{ "text": "SkillEscrow: …" }` for each alert (Slack-compatible), each distinct message at most once an hour. **Must be a valid `https://` URL** (except on `STELLAR_NETWORK=local` where `http://` is accepted). Validated at start-up; malformed URLs cause a configuration error. |

## AI review (optional, advisory)

Needs the GitHub App **and** a key; otherwise reviews are off. It never affects payouts.

| Variable | Default | Notes |
|---|---|---|
| `ANTHROPIC_API_KEY` | none | |
| `AI_MODEL` | `claude-sonnet-5-5` | |
| `AI_TIMEOUT_SECS` | `120` | HTTP timeout for Anthropic requests in seconds. Must be between `1` and `179` (strictly below the 180 s worker job lease). |
| `AI_MAX_REVIEWS_PER_PR_PER_DAY` | `5` | Cost control. |
| `AI_MAX_REVIEWS_PER_REPO_PER_DAY` | `50` | Cost control. |
| `AI_PRICE_INPUT_PER_MTOK` | `3.0` | Cost in USD per million input tokens. Used to compute spend per review and enforce budgets. |
| `AI_PRICE_OUTPUT_PER_MTOK` | `15.0` | Cost in USD per million output tokens. Used to compute spend per review and enforce budgets. |
| `AI_DAILY_BUDGET_USD` | none | Optional daily spend limit in USD. When reached, reviews are skipped for the rest of the day and a comment is posted once per PR. |
| `AI_MONTHLY_BUDGET_USD` | none | Optional monthly spend limit in USD. Tracked alongside daily limit. |
| `REVIEW_DEBOUNCE_SECS` | `300` | Quiet period after a push before reviewing, so a burst costs one review. |

## Worker

| Variable | Default | Notes |
|---|---|---|
| `WORKER_CONCURRENCY` | `4` | Jobs processed in parallel. **Important:** Ensure `DB_MAX_CONNECTIONS` is set to at least `5 + (WORKER_CONCURRENCY × 2)` to prevent connection pool starvation. Each worker may hold database connections during long-running operations (e.g., GitHub API calls). |

## Mainnet differences, in one place

`STELLAR_NETWORK=mainnet` makes:
- `STELLAR_RPC_URL` mandatory (choose a provider deliberately)
- `METRICS_TOKEN` mandatory (protects `/metrics` endpoint)
- `API_PUBLIC_URL` and `WEB_PUBLIC_URL` must be `https://` (enforced at start-up)
- Passphrase and explorer defaults switch to mainnet
- `RELAYER_SECRET` is refused (use KMS-backed signer)
- `doctor` fails (not warns) if security requirements aren't met
