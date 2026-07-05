# Security

What this service defends against, how, and what it does not.

## What is at stake

The escrow contract holds the funds; this service **cannot** move them except through the contract's own rules. What it can do, and therefore what an attacker would want:

1. Make the relayer release a bounty to the wrong address.
2. Make a maintainer sign a transaction they did not intend.
3. Impersonate a user (sessions, wallet links).
4. Spend the operator's money (AI calls, relayer fees) or take the service down.
5. Plant content on GitHub issues in the bot's name.

The contract bounds (1): the relayer can only call `release`, only on a bounty whose maintainer opted in, never to the maintainer, and only while the bounty is open; the amount is capped per bounty. Everything below is about the rest.

## Threats and mitigations

| Threat | Mitigation | Pinned by |
|---|---|---|
| **Forged or replayed GitHub webhook** | HMAC-SHA256 over the **raw** body, constant-time compare, checked before parsing; delivery ids de-duplicated; 10 MiB cap. | `unsigned_forged_or_tampered_webhooks_are_rejected…`, `a_signed_webhook_is_recorded_and_queued_exactly_once` |
| **Relaying an arbitrary transaction** (e.g. `upgrade`, `set_admin`, a call to another contract) through our submit route | `inspect_submission`: signed, standard (no fee bump), single operation, no operation source override, calls **our** contract, function in `{fund_bounty, release, refund, open_dispute, resolve}`, source account matches what the caller said. Refused before the network. | `refuses_calls_to_other_contracts_and_admin_functions`, `refuses_multi_operation_transactions` |
| **Server signing for a user** | It never does: it returns unsigned, simulated XDR. The one key it holds (relayer) signs only `release`. | `build_fund_uses_the_next_sequence_number…` asserts no signatures |
| **Double payout** | One payout row per bounty (unique key); chain re-read before every decision; tx hash stored before waiting; the contract itself rejects a second release. | `payout_flow.rs`, `the_first_merged_pr_wins…` |
| **Payout to the maintainer / self-dealing** | Maintainer-authored PRs are never linked; a winner wallet equal to the maintainer fails the payout; the contract forbids it too. | `the_maintainers_own_pr_never_earns_the_bounty` |
| **Session theft from a database leak** | Session ids are stored **hashed** (SHA-256); the cookie is `HttpOnly`, `SameSite=Lax`, `Secure` over https. | `the_full_github_sign_in_flow…` checks only the hash is stored |
| **CSRF** | A per-session HMAC token required on every state-changing request; CORS allows only `WEB_PUBLIC_URL`. | `sessions_csrf_and_logout` (missing, wrong, and another session's token) |
| **OAuth login CSRF / code interception / open redirect** | PKCE; a signed, expiring state cookie binding the flow to the browser; the return path must be a same-site relative path. | `oauth.rs` tests, `sign_in_failures_are_specific…` |
| **Linking someone else's wallet** | SEP-53 signature over a message binding the GitHub account, the address, a single-use nonce and a 5-minute expiry; the challenge is burned atomically; an address can belong to one account. | `linking_a_wallet_needs_a_real_signature…`, `of_many_simultaneous_verifications_exactly_one_burns_the_challenge` |
| **Funding link abuse** | The link is an unguessable capability, single-use, 24-hour, and only usable by the GitHub user who typed `/bounty`. | `building_the_funding_transaction_checks_who_is_asking` |
| **Path injection into GitHub API calls** via repo name, login, sha or label | Every externally sourced path segment is validated (and `.`/`..` segments rejected) before it is put in a URL. Nothing hostile reaches the network. | `permission_lookup_maps_404_to_none_and_rejects_hostile_input` |
| **Markup / mention injection into bot comments** | Issue text, criteria and model output are escaped before being echoed. | `hostile_criteria_text_cannot_inject_markup_into_the_card` |
| **Prompt injection against the AI reviewer** | Untrusted text fenced and defanged; model told it is data; output must match a strict schema or becomes "needs a human"; code downgrades any review that reports an injection; the reviewer cannot reach the money path. | `ai` tests, `escrowd eval`, `architecture.rs` |
| **Cost abuse** (AI, fees, request floods) | Per-commit review cache, per-PR and per-repo daily caps, debounce; per-user and per-address rate limits; body size caps. | `daily_caps_stop_the_spend`, `mutations_are_rate_limited_per_user_not_shared` |
| **Rate-limit evasion** via `X-Forwarded-For` | The header is ignored unless `TRUST_PROXY` is set. | `x_forwarded_for_is_ignored_unless_the_proxy_is_trusted` |
| **Admin access** | Server-side check on every admin route; the flag follows `ADMIN_GITHUB_IDS` at each login; every admin build is audit-logged; the arbiter's key never touches the server. | `admin_routes_refuse_everyone_but_admins`, `the_admin_flag_follows_configuration_at_every_login` |
| **Secrets in logs** | Secrets are not in `Debug` output; the RPC URL (which can embed an API key) is stripped from network errors; request spans carry the path only (query strings hold one-time OAuth codes). | `signer_debug_hides_the_secret`, `an_unreachable_rpc_is_a_transient_error_that_does_not_leak_the_url` |
| **Supply chain** | `Cargo.lock` committed and `--locked` builds; `cargo audit` in CI; pinned GitHub Action SHAs; gitleaks in CI; dependabot; `#![forbid(unsafe_code)]` in every crate. | CI |
| **SQL injection** | Every query binds its values; the few queries that interpolate text use compile-time constants only (marked `AssertSqlSafe`); `LIKE` input is escaped so wildcards are literal. | `search_treats_wildcards_literally…` |

## Reporting a vulnerability

Please do not open a public issue for a security problem. Contact the repository owners privately (GitHub's "Report a vulnerability" under the Security tab, if enabled).

## Known limits (stated, not hidden)

- **The relayer secret lives in an environment variable on testnet.** That is acceptable for testnet funds only; mainnet refuses it. A KMS-backed signer is required first.
- **The rate limiter is per process.** Several API instances each enforce their own budget.
- **Sessions last 30 days** and are revoked by logout or by deleting the row; there is no per-device session list yet.
- **The audit log is append-only by convention** (the code never updates or deletes it), not by database permissions. Give the application role no `DELETE` on it in production if you want that enforced.
- **Archived contract state** is reported (`state_archived`), not repaired: the restore transaction is not built.
- **GitHub itself is trusted** for who is a collaborator with write access (it gates `/bounty`) and for who authored a PR.
- **No external audit has been done** of this service or the contract.
