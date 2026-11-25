# Moving from the TypeScript backend

This repository still contains the original TypeScript API (Fastify, Prisma, BullMQ) at the root. The Rust backend serves **the same HTTP routes and JSON shapes** and reads **the same environment variable names**, so the web app, the GitHub App, the contract and your `.env` need no changes.

## What differs

| | TypeScript | Rust |
|---|---|---|
| Queue | BullMQ on Redis | Postgres (`FOR UPDATE SKIP LOCKED`, leases, backoff). **`REDIS_URL` is ignored; Redis can be shut down.** |
| Processes | API, worker, Redis, Postgres | `escrowd` (API + worker), Postgres |
| Database | Prisma tables (`"Bounty"`, `"User"`, …) | its own tables (`bounties`, `users`, …) created by `escrowd migrate`; they live side by side with the Prisma tables |
| Event ids / cursor | RPC cursor strings | the same format, so the old cursor can be carried over |

Behavioural improvements you will notice (all covered by tests):

- **Stricter relaying.** `/tx/submit` accepts only a signed, single-operation call to *this* contract's user functions. The TypeScript version only checked "a contract call".
- **Expired submissions are handled.** A relayer transaction that never lands is detected (it has a 2-minute time bound) and resubmitted only if the chain still says the bounty is open; the TypeScript version would wait on the stale hash until attempts ran out.
- **Edited PRs.** Adding `Closes #N` to an existing PR's description links it (the TypeScript version only looked at open/synchronize/reopen).
- **Winner attribution.** On a release the bounty records the winner's GitHub id when their wallet is known.
- **Funding link first.** The funding link is stored before the bot announces it, so a link in a comment always exists.
- **A failed GitHub App configuration waits instead of failing.** Jobs retry until the App is configured.
- Extra endpoints: `GET /config`, `GET /stats`, `GET /metrics`; extra commands: `escrowd doctor`, `reindex`, `eval`, `healthcheck`.

## Cutting over, keeping your data

Bounty state can be rebuilt from the chain, but some things cannot: wallet links, funding links, PR links, reviews, disputes, **the id of each bounty's card comment** (without it the Rust worker would post a second card on every issue), the "already announced/reminded" markers, and the indexer cursor. `scripts/import-from-prisma.sql` carries all of that over.

```bash
# 1. stop the TypeScript API and worker (Redis is no longer needed afterwards)
# 2. build/obtain escrowd and point it at the SAME database (same .env)
escrowd migrate                                   # creates the Rust tables next to the Prisma ones
psql "$DATABASE_URL" -f scripts/import-from-prisma.sql
escrowd doctor
escrowd all
```
The script prints old-versus-new row counts for every table; they should match. It is idempotent (`ON CONFLICT DO NOTHING`) and never modifies the Prisma tables, so you can roll back by stopping `escrowd` and starting the TypeScript service again. Sessions are not copied: people sign in once more.

This was tested against a copy of a real TypeScript deployment's database: all 13 tables matched, the card comment id, maintainer attribution and event markers survived, a second run inserted nothing, and replaying old events through the Rust indexer applied nothing and queued no duplicate cards.

## Starting clean instead

```bash
escrowd migrate
escrowd sync-installations
escrowd reindex --from-ledger <contract deploy ledger>
```
You get every bounty back (for repositories where the App is installed). You lose wallet links, funding links and reviews, and the bot will post a fresh card on each open bounty's issue.

## Running both for a while

You can run the TypeScript API and the Rust API side by side for comparison, each with its own database and port. Do **not** run both workers against one contract with the same relayer key: two signers on one account race on sequence numbers.
