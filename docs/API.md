# HTTP API guide

The authoritative, machine-readable description is [`openapi.yaml`](openapi.yaml) (a test fails the build if it and the router disagree). This page is the human walkthrough: conventions, then the flows you will actually build, with `curl`.

## Conventions

- **JSON everywhere.** 64-bit ids (`id`, `repoId`, `githubId`, …) are **strings**; amounts are **strings** of base units (7 decimals: `"50000000"` is 5 tokens).
- **Errors** are always `{ "error": { "code": "...", "message": "..." } }`. `code` is stable, `message` is for people. Beyond the generic codes (`bad_request`, `unauthenticated`, `forbidden`, `csrf`, `not_found`, `rate_limited`), anything the contract itself rejects comes back as **`contract_<Name>`**: `contract_IssueAlreadyFunded`, `contract_DeadlineNotReached`, `contract_AmountTooSmall`, and so on (the full list is in `escrow-core::contract_errors`). Network trouble is a `503` (`chain_unavailable`), never blamed on the caller.
- **Sessions** are the `HttpOnly` cookie `se_session`. Every state-changing request also needs the CSRF token from `GET /auth/me` in the `X-CSRF-Token` header.
- **Limits**: 300 requests per minute per address overall (`/health` exempt); 20 state-changing requests per minute per user (30 for admin routes). Over the limit: `429 rate_limited`. Request bodies are capped at 1 MiB (10 MiB for webhooks).
- **CORS** allows only `WEB_PUBLIC_URL`, with credentials.

## Reading bounties (no sign-in)

```bash
curl 'localhost:3001/bounties?sort=highest&limit=2'
```
```json
{
  "items": [{
    "id": "4", "repoId": "1441828628", "issueNumber": 3, "issueTitle": "Retry failed HTTP requests",
    "issueUrl": "https://github.com/QueenGiba/skillescrow-sandbox/issues/3", "criteria": ["Retries up to 3 times"],
    "maintainer": "GDBD5CJ4…", "token": "CDLZFC3S…", "amount": "50000000", "feeBps": 300,
    "deadline": "2026-11-04T11:57:47Z", "relayerAllowed": true, "status": "OPEN",
    "winnerAddress": null, "fundTx": "8278f51f…", "closeTx": null,
    "repo": { "fullName": "QueenGiba/skillescrow-sandbox" }, "_count": { "prs": 0 }
  }],
  "nextCursor": null
}
```

Filters: `status` (default `OPEN`), `token`, `minAmount`, `q` (title or repository, case-insensitive, wildcards are literal), `sort` (`newest` | `highest` | `ending`), `limit` (1–50), `cursor` (the previous `nextCursor`). Pagination is stable under ties. `GET /bounties/{id}` adds the pull requests (each with its latest AI review) and the payouts. `GET /config` returns the fee, grace and durations read live from the contract plus the arbiter's address; `GET /stats` returns headline numbers; `GET /users/{login}` is a contributor's public record (confirmed payouts only, each verifiable on chain by its `txHash`).

## Signing in

```
browser ─▶ GET /auth/login?next=/b/4
           303 → GitHub (authorization code + PKCE); sets a signed, expiring se_oauth cookie
GitHub  ─▶ GET /auth/callback?code=…&state=…
           verifies the cookie and state, exchanges the code with the PKCE verifier,
           upserts the user (admin flag from ADMIN_GITHUB_IDS), sets se_session, 303 → WEB_PUBLIC_URL/b/4
browser ─▶ GET /auth/me        { id, login, avatarUrl, wallet, isAdmin, csrfToken }
```

`next` must be a same-site path; anything else becomes `/me`. Failures are specific (`bad_state`, `state_expired`, `github_rejected`, `github_unavailable`) and clear the state cookie. Until the App's OAuth credentials are configured, `/auth/login` answers `501 login_not_configured`.

## Linking a wallet

```bash
# 1. ask for a message (needs the cookie and X-CSRF-Token)
curl -X POST localhost:3001/wallet/challenge -b 'se_session=…' -H 'x-csrf-token: …' \
     -H 'content-type: application/json' -d '{"address":"G…"}'
# → { "challengeId": "…", "message": "SkillEscrow wallet link\nGitHub: octocat (583231)\nAddress: G…\nNonce: …\nExpires: …" }

# 2. the wallet signs it with signMessage (SEP-53), then:
curl -X POST localhost:3001/wallet/verify … -d '{"challengeId":"…","signature":"<base64>"}'
# → { "address": "G…" }
```

The signature is verified over `sha256("Stellar Signed Message:\n" + message)`. A challenge expires after 5 minutes and is single-use (burned atomically). An address already linked to another account is refused (`address_taken`). If a payout was waiting for this wallet, it continues immediately.

## Funding a bounty

The bot's comment links to the web app's `/fund/<id>`, which uses:

```
GET  /intents/{id}                 { state: open|used|expired, repo, issueNumber, token, amount, durationDays, expiresAt, bountyId }
POST /intents/{id}/build           { address, relayerAllowed }  →  { xdr }      unsigned, simulated, ready for the wallet
     (wallet signs the XDR)
POST /tx/submit                    { xdr, address? }            →  { hash }
```

Only the GitHub user who typed `/bounty` may build through the link (`403 not_requester`). `/tx/submit` accepts **only** a signed, single-operation call to this contract's user functions; an unsigned envelope, another contract, a fee bump, a muxed source, or an admin function such as `upgrade` is refused before it reaches the network. After submission the indexer sees the `funded` event, creates the bounty, and the card appears on the issue.

## Releasing and refunding as the maintainer

```
POST /bounties/{id}/release/build   →  { xdr, winner }   only for a merged PR whose payout is AWAITING_MAINTAINER
POST /bounties/{id}/refund/build    →  { xdr }           the contract enforces deadline + 72 h grace
```

Both require your linked wallet to be the bounty's maintainer, and both re-read the bounty from the chain first (`409 not_open` if it is no longer open).

## Admin (the arbiter)

Admin rights come from `ADMIN_GITHUB_IDS` and are re-applied at every login. The arbiter's key never touches the server: these routes only prepare transactions for the arbiter's wallet.

```
GET  /admin/disputes                           open disputes, oldest first (overdue = older than 36 h)
POST /admin/disputes/{id}/open-onchain         { arbiterAddress }                       → { xdr }   open_dispute
POST /admin/disputes/{id}/resolve/build        { arbiterAddress, winner?, winnerBps, note? } → { xdr }   resolve (a split)
POST /admin/disputes/{id}/reject               { note }
GET  /admin/payouts?status=FAILED
POST /admin/payouts/{id}/retry                 only FAILED payouts with no submitted transaction
GET  /admin/status
```

`winnerBps` is the winner's share in basis points (0–10000); a winner is required when it is above 0, and at 0 the whole amount returns to the maintainer. A payout that has a transaction hash cannot be retried (`409 has_tx_hash`): verify it on chain instead, so a retry can never pay twice. Every admin build is written to the audit log.

## GitHub webhooks

`POST /github/webhooks` with `X-Hub-Signature-256`, `X-GitHub-Delivery`, `X-GitHub-Event`. The signature is checked over the raw body before anything is parsed. A new delivery is recorded and queued (`202 { "queued": true }`); a redelivery is acknowledged without being processed again (`200 { "duplicate": true }`). Handled events: `installation`, `installation_repositories`, `issue_comment` (commands), `pull_request` (linking, review scheduling, merge → payout), `issues` (title and criteria edits). Others are acknowledged and ignored.

## Operations

- `GET /health`: `{ ok, db, rpc, indexerLagLedgers, deadJobs }`. `503` when the database or RPC is unreachable or the indexer is more than 60 ledgers behind.
- `GET /metrics`: Prometheus text. Counters for requests, 5xx, rate-limited requests, webhooks accepted / duplicate / rejected, sign-ins, wallets linked; gauges for bounties, payouts and jobs by status and the indexer's ledger. Set `METRICS_TOKEN` to require `Authorization: Bearer …`.
