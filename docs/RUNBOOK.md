# Runbook

Operating the backend: what to watch, what each alert means, how to recover, and what to do before real money.

- [Daily](#daily)
- [Alerts and what to do](#alerts-and-what-to-do)
- [Common situations](#common-situations)
- [Backups and recovery](#backups-and-recovery)
- [Rotating secrets](#rotating-secrets)
- [Before mainnet](#before-mainnet)

## Daily

```bash
escrowd doctor                         # every dependency, with fixes (docker compose exec app escrowd doctor)
curl -s https://API/health             # { ok, db, rpc, indexerLagLedgers, deadJobs }
```
As an admin, `GET /admin/status` gives the one-glance summary: indexer ledger, failed and waiting payouts, open disputes, dead and pending jobs. `/metrics` has the same as Prometheus gauges.

## Alerts and what to do

The worker checks every minute and sends each distinct message at most once an hour to `ALERT_WEBHOOK_URL` (and always logs it at `ERROR`).

| Alert | Meaning | Do |
|---|---|---|
| `Relayer balance low: N XLM` | The relayer pays transaction fees; at zero, automatic payouts stop. | Top up the relayer address (shown by `doctor`). On testnet use Friendbot. |
| `N payout(s) FAILED` | A payout hit a contract rejection or ran out of retries. | `GET /admin/payouts` and read `lastError`. See [a payout failed](#a-payout-failed). |
| `Indexer is N ledgers behind` | Events are not being read. Usually Soroban RPC trouble. | Check `doctor` and the worker log. See [the indexer is behind](#the-indexer-is-behind). |
| `N dispute(s) older than 36 h` | The promised response time has passed. | Work the queue: `GET /admin/disputes`. |
| `Bounty N has a merged PR (STATUS) and its refund window is near` | A merged PR is unpaid and the maintainer can reclaim the funds within 72 h of the deadline. | Unblock the payout (usually `PENDING_WALLET`: ask the contributor to link a wallet; or the maintainer must sign). |
| `N background job(s) gave up and are dead` | A job exhausted its attempts or can never succeed. | See [dead jobs](#dead-jobs). |

## Common situations

### A payout failed

```sql
SELECT id, bounty_id, status, tx_hash, attempts, last_error FROM payouts WHERE status = 'FAILED';
```
- `last_error` names a contract error (e.g. `RelayerNotAllowed`, `InvalidStatus`): the contract said no, and retrying cannot help. Inspect the bounty on chain; the bounty was probably refunded, disputed or already paid by another route.
- `tx_hash` is set: **do not retry blindly.** Look the hash up on a Stellar explorer. If it succeeded the indexer will confirm it from the `released` event; if it failed or expired and the bounty is still open, `POST /admin/payouts/{id}/retry` is refused (`has_tx_hash`) on purpose, so ask a maintainer to release it themselves or clear the hash after verifying.
- No hash and the error is transient (network, fees): `POST /admin/payouts/{id}/retry` re-queues it. Check the relayer's balance first.

### A contributor was never paid after a merge

Check the payout row for that bounty: `PENDING_WALLET` (they have not linked a wallet; the bot already asked on the PR; once they link it, it continues by itself), `AWAITING_MAINTAINER` (the maintainer funded without relayer permission and must sign: the bounty page has the button), `QUEUED`/`SUBMITTED` (in flight; check the relayer log), `FAILED` (above). If there is **no** payout row, the merge webhook was not processed: look at GitHub's "Recent deliveries" for the App, redeliver, and check `webhook_deliveries` and `jobs`.

### The indexer is behind

```bash
docker compose logs --tail=100 app | grep -i -E "indexer|rpc"
escrowd doctor
```
- RPC errors or timeouts: the provider is down or rate limiting you. The indexer retries on its own; consider a second provider via `STELLAR_RPC_URL`.
- `cursor outside the retained range`: the worker was down longer than the RPC keeps events. It drops the cursor and restarts from the fallback. Run `escrowd reindex --from-ledger N` with the oldest ledger you still need (events older than the RPC's retention cannot be read; the contract's state is still correct on chain, and `get_bounty` is what payouts use).
- A log line about a malformed event: a tracked event could not be decoded, so the indexer **stopped advancing on purpose** rather than index something wrong. Do not skip it: investigate (did the contract change?).

### Dead jobs

```sql
SELECT id, kind, attempts, last_error, updated_at FROM jobs WHERE status = 'dead' ORDER BY id;
```
`bad webhook payload` means GitHub sent something we cannot read: nothing to retry. `GitHub App is not configured` or 5xx errors mean the cause was transient and has probably passed: re-queue.
```sql
UPDATE jobs SET status = 'queued', attempts = 0, run_at = now(), locked_until = NULL WHERE id = <id>;
```
Card refreshes and "Paid" announcements are idempotent, so re-queueing them is always safe.

### A bounty card is missing or stale

Its job may be dead or waiting for GitHub. Enqueue a refresh:
```sql
INSERT INTO jobs (kind, payload, max_attempts, dedupe_key)
VALUES ('card.refresh', '{"bountyId": 4}', 8, 'card:4');
```

### Webhooks are not arriving

GitHub App → Advanced → Recent deliveries. `401` from us means the secret differs from `GITHUB_WEBHOOK_SECRET`; connection errors mean DNS, TLS or the host's firewall. A redelivery is safe (de-duplicated by delivery id).

### The API is up but the worker is not

`/health` stays green but cards, commands and payouts stop. `GET /admin/status` shows `pendingJobs` growing and the indexer ledger not moving. Restart the worker (`docker compose restart app`).

## Backups and recovery

**What the chain can restore, and what it cannot:**

| Can be rebuilt | Needs a backup |
|---|---|
| Bounty state: `escrowd reindex --from-ledger N` | Users and wallet links, funding links, PR ↔ bounty links, reviews, disputes, the audit log, the card comment ids |
| Installations and repositories: `escrowd sync-installations` | Webhook delivery history |

Back up the whole database daily; it is small.
```bash
# cron, on the host
docker compose exec -T db pg_dump -U escrow -Fc escrow > /backups/escrow-$(date +%F).dump
find /backups -name 'escrow-*.dump' -mtime +14 -delete
```
Copy the dumps off the host (object storage or another machine). **A backup you have not restored is a hope, not a backup**: restore into a scratch database regularly:
```bash
createdb escrow_restore && pg_restore -d escrow_restore --no-owner /backups/escrow-YYYY-MM-DD.dump
DATABASE_URL=postgres://…/escrow_restore escrowd doctor
```

**Total loss of the database:** create a fresh one, `escrowd migrate`, restore the latest dump if you have one, then `escrowd sync-installations` and `escrowd reindex --from-ledger <contract deploy ledger or the oldest the RPC still holds>`. Without a dump, bounty state returns but wallet links must be redone by contributors and the bot will post new cards (the old ones remain on the issues).

## Rotating secrets

| Secret | How |
|---|---|
| `SESSION_SECRET` | Change and restart. Only in-flight sign-ins (10-minute state cookie) are affected; sessions are random ids in the database and stay valid. |
| `CSRF_SECRET` | Change and restart. Every browser needs to reload once to fetch a fresh token. |
| `GITHUB_WEBHOOK_SECRET` | Change in the GitHub App settings and in the environment together, then restart. Deliveries sent in the gap get `401`; redeliver them. |
| GitHub App private key | Generate a new key in the App settings, update `GITHUB_APP_PRIVATE_KEY`, restart, then delete the old key in GitHub. |
| `RELAYER_SECRET` | Register the new relayer address in the contract (admin `set_relayer`), fund it, update the variable, restart. Until the contract is updated, `doctor` and the worker will report the mismatch and automatic payouts stay off. |
| `ANTHROPIC_API_KEY` | Change and restart. |

## Before mainnet

Not optional, in rough order:

1. **A KMS-backed relayer signer** (implement `ReleaseSigner`; the config already refuses `RELAYER_SECRET` on mainnet). The relayer can only release bounties whose maintainer opted in, but a leaked key can still redirect those.
2. **A merge-to-payout run on testnet with this backend** (the one open verification), and a refund and a dispute run.
3. **An external audit** of the contract and a review of this service's money path.
4. **A staging soak** of at least a week on testnet with real maintainers.
5. **Backups with a tested restore**, off-host.
6. An **admin multisig** for the contract's admin and arbiter roles.
7. Error tracking, uptime monitoring and the alert webhook wired to a channel a human reads.
8. A named RPC provider with an SLA (`STELLAR_RPC_URL` is mandatory on mainnet for this reason) and a fallback.
9. Per-bounty caps kept low at first (`TOKEN_MAX_AMOUNT`, and the contract's own limit).
