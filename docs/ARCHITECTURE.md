# Architecture

How the backend is put together, why, and what happens when things go wrong.

- [The shape](#the-shape)
- [Components](#components)
- [Data model](#data-model)
- [The indexer](#the-indexer)
- [The payout path](#the-payout-path)
- [The job queue](#the-job-queue)
- [GitHub integration](#github-integration)
- [The AI reviewer](#the-ai-reviewer)
- [Failure handling](#failure-handling)
- [Design decisions](#design-decisions)

## The shape

```
                    ┌────────────────────────── escrowd (one process, or split) ───────────────────────────┐
  GitHub ──webhook──▶  API (axum)                                        WORKER                           │
                    │   verify HMAC ─▶ [deliveries + jobs]  ─────▶  job queue loop ─▶ dispatch handlers     │
  web app ──REST────▶   auth, CSRF, limits, reads                       │  commands · PRs · installations    │
                    │   prepares UNSIGNED txs, relays signed ones       │  card sync · "Paid" comments       │
                    │                                                   ▼                                    │
                    │                                              INDEXER ◀──events── Soroban RPC           │
                    │                                              only writer of bounty status             │
                    │                                              enqueues card/announce jobs, same tx     │
                    │                                                   │                                    │
                    │                                              RELAYER LOOP ──release()──▶ Soroban RPC   │
                    │                                              one at a time, hash stored before wait   │
                    └──────────────────────────────┬─────────────────────────────────────────────────────────┘
                                                   ▼
                                              PostgreSQL  (cache of the chain + GitHub bookkeeping + the job queue)
```

`escrowd all` runs the API and the worker in one process. `escrowd serve` and `escrowd worker` split them; both can run as several instances because every coordination point is in Postgres (row locks, leases, unique indexes), with one exception: the **relayer loop** assumes a single signing account, so run exactly one worker that has `RELAYER_SECRET`.

## Components

| Component | Crate | Job |
|---|---|---|
| **Core** | `escrow-core` | Pure logic with no I/O: amounts and fees (mirroring the contract's arithmetic exactly), the `/bounty` grammar, acceptance-criteria and closing-keyword parsing, the bounty card, webhook HMAC, CSRF, PKCE and OAuth state, config. |
| **Chain** | `escrow-chain` | A small Soroban RPC client; transaction building (invoke, simulate, assemble, sign); `ScVal` encoding; event decoding; the **submission allow-list**; the relayer. Everything is behind the traits in `ports.rs`. |
| **DB** | `escrow-db` | The schema (one migration) and one module of explicit queries per area. Statuses are text with `CHECK` constraints; amounts are `NUMERIC(39,0)` and travel as decimal strings. The job queue is here too. |
| **GitHub** | `escrow-github` | GitHub App auth (JWT → installation token, cached, refreshed on 401), REST calls with validated path segments, OAuth calls, typed webhook payloads. Behind the `GithubApi` trait. |
| **Payouts** | `escrow-payouts` | `on_merge`, `advance`, `relay_release`, and the relayer loop. The money state machine. |
| **AI** | `escrow-ai` | The advisory reviewer. Cannot reach the money path and is not reachable from it. |
| **Engine** | `escrow-engine` | Indexer, command / PR / webhook handlers, card sync, reminders, alerts, health, and the worker (queue loop + periodic tasks). |
| **API** | `escrow-api` | The axum router, sessions, CSRF, wallet linking, funding, admin, webhook receiver, metrics. |
| **escrowd** | `escrowd` | Wires configuration into running parts; subcommands; `doctor`. The only place that knows every crate. |

The dependency rules between them are tests (`crates/escrowd/tests/architecture.rs`), not conventions. If one fails, the design is being violated.

## Data model

One migration, `crates/escrow-db/migrations/0001_init.sql`. The tables, grouped by what they are:

**The chain's cache** (rebuildable by re-indexing)

| Table | Notes |
|---|---|
| `bounties` | One row per bounty, keyed by the contract's id. `status` is written only by the indexer; `last_event_id` makes event application idempotent. Enriched with `issue_title`, `criteria`, `card_comment_id`, `maintainer_github_id`. |
| `indexer_cursors` | The RPC cursor and the last ledger reached, saved in the same transaction as the events it covers. |

**GitHub bookkeeping** (cannot be rebuilt from the chain; back this up)

| Table | Notes |
|---|---|
| `users`, `sessions`, `wallet_challenges` | GitHub identity, the linked wallet, and sign-in. Session ids are stored hashed. |
| `installations`, `repos` | Where the App is installed. Reconcilable from GitHub (`escrowd sync-installations`). |
| `funding_intents` | What `/bounty` created: token, amount, duration, who asked, expiry, used-at. |
| `pull_requests`, `pull_request_bounties` | PRs and their links. A PR can close several bountied issues: one link per (PR, bounty). |
| `ai_reviews` | One row per (PR, commit). Advisory. Carries the id of the single GitHub comment so a re-review edits it. |
| `disputes`, `payouts` | The arbiter's queue and the payout state machine (below). |
| `webhook_deliveries` | De-duplication of GitHub redeliveries. |
| `audit_log` | Append-only; also the "already sent" memory for reminders and announcements. |

**Plumbing**: `jobs`, the durable queue.

Conventions worth knowing: ids from GitHub and the contract are 64-bit integers (`BIGINT`) and travel to JSON as **strings**; amounts are exact integers of base units, never floats; every status column has a `CHECK` constraint listing its values, so a typo is an error rather than a silent new state.

## The indexer

`escrow-engine::indexer`. It folds the contract's events into the database and is the **only writer of bounty status**.

```
loop:
  cursor = saved cursor        (or: INDEXER_START_LEDGER, or tip − INDEXER_LOOKBACK_LEDGERS on a new database)
  page   = getEvents(cursor)   (200 per page)
  BEGIN
    for each event in the page:
      skip if the contract call failed
      decode it                (a malformed TRACKED event aborts the whole page, loudly)
      apply it:                funded | released | refunded | disputed | resolved
        stale     the bounty already applied this event or a later one  → no-op
        untracked a repository we do not have installed                  → ignored
        anomaly   contradicts known state (e.g. release on a refunded bounty) → ignored, counted, logged
        applied   → also enqueue `card.refresh` (and `payout.announce` for a release) IN THIS TRANSACTION
    save cursor + ledger        (the ledger follows the CURSOR, so a quiet contract still looks alive)
  COMMIT
  until the cursor reaches the tip
```

Why it is safe:

- **One transaction per page** covers the events, the follow-up jobs and the cursor. A crash replays at most one page; replay is harmless because each bounty records the last event id it applied and an older event is recognised as stale. (`replaying_a_page_changes_nothing_and_queues_nothing_more`.)
- **Event ids sort in chain order** (zero-padded), so "have I applied this?" is a string comparison.
- **The RPC returns empty pages with advanced cursors** over quiet ranges, so the loop pages until the cursor reaches the latest ledger rather than stopping at the first empty page. (`it_keeps_paging_until_the_cursor_reaches_the_tip`.)
- **A cursor older than the RPC's retention** is detected, dropped with a warning, and the next poll restarts from the fallback instead of failing forever. (`a_cursor_older_than_the_rpcs_retention…`)
- **Funding attributes itself**: a `funded` event is matched to the newest unused funding intent with the same repo, issue, token and exact amount, which is how the bounty learns which GitHub user is its maintainer.

## The payout path

`escrow-payouts`. Statuses: `PENDING_WALLET → QUEUED | AWAITING_MAINTAINER → SUBMITTED → CONFIRMED`, or `FAILED`.

```
merge webhook ─▶ on_merge
   chain says the bounty is not OPEN?                    → stop (NotOpen)
   INSERT payout, idempotency key release:<bounty>
      key already exists?                                → "another PR already claimed this" (first merge wins)
   advance:
      chain re-read: still OPEN?
      contributor has a linked wallet?                   no  → PENDING_WALLET, ask on the PR
      wallet == the maintainer's?                        yes → FAILED  (the contract forbids self-payout anyway)
      maintainer allowed the relayer?                    yes → QUEUED
                                                         no  → AWAITING_MAINTAINER, tell them where to sign
      (guarded UPDATE: of several concurrent callers exactly one moves it)

relayer loop (one signer, one payout at a time):
   relay_release:
      CONFIRMED / FAILED already?                        → done
      status SUBMITTED with a stored hash?               → wait on THAT hash; never submit again
      chain: already RELEASED to our winner?             → CONFIRMED (we crashed after it landed)
      chain: not OPEN, or relayer not allowed?           → FAILED
      submit release()                                   → store the hash  ← BEFORE waiting
      wait for the transaction
         SUCCESS    → CONFIRMED
         FAILED     → FAILED
         not found  → seen recently? keep waiting (transient)
                      older than 5 min (it expired and can never land)?
                         chain says paid → CONFIRMED;  chain still OPEN → forget the hash, QUEUED, resubmit
      failure kinds:
         contract rejection (a named contract error)     → FAILED now, no retry
         anything else                                   → retry with backoff (2 s, 4 s … 5 min), FAILED after 5 attempts
```

The relayer's transactions carry a **2-minute time bound**. That is what makes "expired" decidable: a transaction older than that cannot land any more, so resubmitting cannot double-pay, and even if it somehow could, the contract rejects a second release (`InvalidStatus`).

When the indexer later sees the `released` event it marks the payout `CONFIRMED` with the **chain's** transaction hash and queues the "Paid …" comment. That covers every release path: the relayer, the maintainer signing, or a dispute resolution.

## The job queue

`escrow-db::jobs` (the table) and `escrow-engine::worker` (the loop). It replaces Redis/BullMQ.

- **Enqueue in the same transaction as the cause.** The webhook receiver inserts the delivery row and its job together; the indexer enqueues card refreshes with the events. Neither can exist without the other.
- **Claim** with `UPDATE … WHERE id = (SELECT … FOR UPDATE SKIP LOCKED)`: many workers, no double-claims. A claim is a **lease** (3 minutes). A worker that dies lets its lease lapse and the job is claimed again, until its attempts run out.
- **Retry** with exponential backoff (4 s, 8 s … 15 min). A failure that can never succeed (a malformed payload, a permission failure) is **buried immediately** as `dead` with its reason, not retried five times. `GithubError::NotConfigured` is deliberately retryable, so jobs wait for the App to be configured.
- **Dedupe and debounce**: a unique partial index on `dedupe_key` while `status = 'queued'`. Card refreshes collapse (many events for one bounty = one refresh). Review requests **debounce**: a newer push moves the waiting review later instead of adding another (5-minute quiet period).
- **Observability**: `dead` jobs raise an alert and appear in `/health`, `/admin/status` and `/metrics`.

## GitHub integration

- **Auth**: an App JWT (RS256, back-dated 30 s for skew, 9-minute lifetime) is exchanged for an installation token, cached until a minute before expiry, refreshed once on a 401.
- **Webhooks** are acknowledged immediately after the signature check and processed later, so GitHub never times out on us and a restart loses nothing.
- **The card**: one comment per bounty, posted once and edited in place. A transaction-scoped advisory lock (`pg_advisory_xact_lock`) serialises refreshes; without it two concurrent refreshes would both see "no card yet" and both post one. (`concurrent_refreshes_post_exactly_one_card`.)
- **Commands** are read from the first line of a comment only, so quoted text can never trigger one. Funding needs write access, an open issue (not a PR) and no active bounty.

## The AI reviewer

Advisory and isolated. Defence in layers: untrusted text is **fenced** (and the fence tags inside it are defanged) before it reaches the model; the system prompt says the fenced content is data and to flag injection attempts; the model must answer through a **forced tool call** whose output is validated against a strict schema (anything else, twice, becomes "needs a human", never a guess); and **code, not the model**, downgrades a review that reports an injection (never "likely meets", score capped at 40). Duplicate-diff and failing-CI flags are computed by code, so the model cannot remove them. Costs are bounded by a per-commit cache and per-PR / per-repo daily caps. `escrowd eval` runs 12 cases (including prompt-injection attacks) against the real model; run it before changing the prompt or the model.

## Failure handling

| Failure | What happens |
|---|---|
| Process killed mid-page (indexer) | The page's transaction rolls back; the next poll re-reads it; stale detection makes replay harmless. |
| Process killed after `release()` was submitted | The hash was stored first. Restart waits on that hash; it never submits again. |
| Process killed after the release landed but before we recorded it | `relay_release` reads the chain first and confirms. |
| Submitted transaction never appears | After 5 minutes it provably cannot land; if the chain still says OPEN it is resubmitted. |
| Soroban RPC down | The indexer and relayer retry; `/health` goes 503 once the indexer is 60+ ledgers behind; alerts fire. |
| GitHub down or rate-limited | Jobs retry with backoff; payouts are unaffected (comments are best effort). |
| GitHub App not configured | The API and indexer still run; GitHub-dependent jobs wait (retryable). |
| Database down | `/health` 503; jobs and the indexer resume when it returns. Nothing is acknowledged that was not stored. |
| Malformed tracked contract event | The page is rolled back and the indexer stops advancing, loudly, rather than indexing something wrong. |
| Lost database | Re-index (`escrowd reindex`) restores bounty state; restore a backup for GitHub bookkeeping (see the runbook). |
| Webhook redelivered | Recognised by delivery id; acknowledged, not processed twice. |
| Two merges for one bounty | One payout row (unique key); the second PR is told so. |

## Design decisions

- **Postgres instead of Redis for the queue.** One service fewer to run, host and back up; atomicity with the data (a job is enqueued in the same transaction as its cause); and the volume here (webhooks, card refreshes, reviews) is tiny. The trade-off is that Postgres is the only moving dependency and a very high job rate would eventually want a dedicated broker. That is not this workload.
- **Runtime-checked SQL (no build-time database).** The build needs no database or offline query cache, and every query is exercised by a test against real Postgres. The cost is that a typo is caught by tests, not the compiler.
- **Traits at every seam** (`ChainReader`, `EventSource`, `TxBuilder`, `ReleaseSigner`, `GithubApi`, `ModelPort`, `Notifier`). Production wires real implementations; tests wire in-memory ones, which is why 337 tests need no network. It is also how a KMS signer will slot in.
- **Crate boundaries as architecture.** The cheapest way to guarantee "the AI cannot affect payouts" is for it to be impossible to link, and for a test to say so.
- **A hand-written Soroban RPC client over `stellar-xdr`** rather than a large SDK: a few JSON-RPC methods, XDR types from the official crate, and full control of what we sign and submit. The only key the process ever holds signs one kind of transaction.
- **The same HTTP contract as the TypeScript API**, so the web app, the contract and the docs did not have to change.
