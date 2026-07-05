-- One-shot import of the TypeScript backend's data (Prisma tables) into the Rust backend's tables.
--
-- Why this exists: bounty state can be rebuilt from the chain (`escrowd reindex`), but some things cannot:
--   * wallet links and GitHub users, funding links, PR <-> bounty links, reviews, disputes
--   * the id of each bounty's card comment (without it the Rust worker would post a SECOND card on every issue)
--   * the "already announced / already reminded" markers (without them it would repeat comments)
--   * the indexer cursor (so indexing continues exactly where the old one stopped)
--
-- Usage (the Prisma tables and the Rust tables live side by side: "User" vs users):
--   1. stop the TypeScript API and worker
--   2. escrowd migrate                       # creates the Rust tables in the SAME database
--   3. psql "$DATABASE_URL" -f scripts/import-from-prisma.sql
--   4. escrowd doctor && escrowd all
--
-- Idempotent: every insert is ON CONFLICT DO NOTHING, so re-running it is harmless. Sessions are not copied
-- (people simply sign in again). Nothing in the Prisma tables is modified or deleted.

BEGIN;

INSERT INTO users (id, github_id, github_login, avatar_url, wallet, wallet_linked_at, is_admin, created_at, updated_at)
SELECT id, "githubId", "githubLogin", "avatarUrl", wallet, "walletLinkedAt", "isAdmin", "createdAt", "updatedAt" FROM "User"
ON CONFLICT DO NOTHING;

INSERT INTO installations (id, account_login, account_type, suspended_at, created_at)
SELECT id, "accountLogin", "accountType", "suspendedAt", "createdAt" FROM "Installation"
ON CONFLICT DO NOTHING;

INSERT INTO repos (id, installation_id, full_name, is_private)
SELECT id, "installationId", "fullName", private FROM "Repo"
ON CONFLICT DO NOTHING;

INSERT INTO funding_intents (id, repo_id, issue_number, requested_by, token, amount, duration_days, comment_id, expires_at, used_at, created_at)
SELECT id, "repoId", "issueNumber", "requestedBy", token, amount, "durationDays", "commentId", "expiresAt", "usedAt", "createdAt" FROM "FundingIntent"
ON CONFLICT DO NOTHING;

INSERT INTO bounties (id, repo_id, issue_number, issue_title, issue_url, criteria, maintainer, maintainer_github_id, token, amount, fee_bps,
                      deadline, relayer_allowed, status, winner_address, winner_github_id, fund_tx, close_tx, card_comment_id, last_event_id,
                      created_at, updated_at)
SELECT id, "repoId", "issueNumber", "issueTitle", "issueUrl", criteria, maintainer, "maintainerGithubId", token, amount, "feeBps",
       deadline, "relayerAllowed", status::text, "winnerAddress", "winnerGithubId", "fundTx", "closeTx", "cardCommentId", "lastEventId",
       "createdAt", "updatedAt" FROM "Bounty"
ON CONFLICT DO NOTHING;

INSERT INTO pull_requests (id, number, author_github_id, author_login, head_sha, state, merged_at, diff_hash)
SELECT id, number, "authorGithubId", "authorLogin", "headSha", state, "mergedAt", "diffHash" FROM "PullRequest"
ON CONFLICT DO NOTHING;

INSERT INTO pull_request_bounties (pr_id, bounty_id)
SELECT "prId", "bountyId" FROM "PullRequestBounty"
ON CONFLICT DO NOTHING;

INSERT INTO ai_reviews (id, pr_id, head_sha, model, verdict, score, result, flags, input_tokens, output_tokens, comment_id, created_at)
SELECT id, "prId", "headSha", model, verdict, score, result, flags, "inputTokens", "outputTokens", "commentId", "createdAt" FROM "AiReview"
ON CONFLICT DO NOTHING;

INSERT INTO payouts (id, bounty_id, pr_number, winner_github_id, winner_address, status, idempotency_key, tx_hash, attempts, last_error, created_at, updated_at)
SELECT id, "bountyId", "prNumber", "winnerGithubId", "winnerAddress", status::text, "idempotencyKey", "txHash", attempts, "lastError", "createdAt", "updatedAt" FROM "Payout"
ON CONFLICT DO NOTHING;

INSERT INTO disputes (id, bounty_id, opened_by_github_id, reason, evidence, status, resolution_note, created_at, updated_at)
SELECT id, "bountyId", "openedByGithubId", reason, evidence, status, "resolutionNote", "createdAt", "updatedAt" FROM "Dispute"
ON CONFLICT DO NOTHING;

INSERT INTO webhook_deliveries (id, event, received_at, processed_at, error)
SELECT id, event, "receivedAt", "processedAt", error FROM "WebhookDelivery"
ON CONFLICT DO NOTHING;

INSERT INTO indexer_cursors (id, cursor, ledger, updated_at)
SELECT id, cursor, ledger, "updatedAt" FROM "IndexerCursor"
ON CONFLICT DO NOTHING;

INSERT INTO audit_log (id, actor, action, target, data, created_at)
SELECT id, actor, action, target, data, "createdAt" FROM "AuditLog"
ON CONFLICT DO NOTHING;

-- A summary you can eyeball: each pair should match (old = new).
SELECT 'users' AS what, (SELECT count(*) FROM "User") AS old, (SELECT count(*) FROM users) AS new
UNION ALL SELECT 'installations', (SELECT count(*) FROM "Installation"), (SELECT count(*) FROM installations)
UNION ALL SELECT 'repos', (SELECT count(*) FROM "Repo"), (SELECT count(*) FROM repos)
UNION ALL SELECT 'funding_intents', (SELECT count(*) FROM "FundingIntent"), (SELECT count(*) FROM funding_intents)
UNION ALL SELECT 'bounties', (SELECT count(*) FROM "Bounty"), (SELECT count(*) FROM bounties)
UNION ALL SELECT 'pull_requests', (SELECT count(*) FROM "PullRequest"), (SELECT count(*) FROM pull_requests)
UNION ALL SELECT 'pull_request_bounties', (SELECT count(*) FROM "PullRequestBounty"), (SELECT count(*) FROM pull_request_bounties)
UNION ALL SELECT 'ai_reviews', (SELECT count(*) FROM "AiReview"), (SELECT count(*) FROM ai_reviews)
UNION ALL SELECT 'payouts', (SELECT count(*) FROM "Payout"), (SELECT count(*) FROM payouts)
UNION ALL SELECT 'disputes', (SELECT count(*) FROM "Dispute"), (SELECT count(*) FROM disputes)
UNION ALL SELECT 'webhook_deliveries', (SELECT count(*) FROM "WebhookDelivery"), (SELECT count(*) FROM webhook_deliveries)
UNION ALL SELECT 'indexer_cursors', (SELECT count(*) FROM "IndexerCursor"), (SELECT count(*) FROM indexer_cursors)
UNION ALL SELECT 'audit_log', (SELECT count(*) FROM "AuditLog"), (SELECT count(*) FROM audit_log);

COMMIT;
