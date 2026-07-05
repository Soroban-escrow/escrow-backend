-- SkillEscrow schema. The database is a CACHE of the chain plus the GitHub-side bookkeeping the chain
-- cannot know (installations, funding links, PR links, reviews). Bounty status is written only by the
-- indexer from contract events, so a lost database can be rebuilt by re-indexing.

CREATE TABLE users (
  id               TEXT PRIMARY KEY,
  github_id        BIGINT NOT NULL UNIQUE,
  github_login     TEXT NOT NULL,
  avatar_url       TEXT,
  wallet           TEXT UNIQUE,
  wallet_linked_at TIMESTAMPTZ,
  is_admin         BOOLEAN NOT NULL DEFAULT FALSE,
  created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX users_login_lower ON users (lower(github_login));

-- session ids are stored HASHED: a leaked table is not a set of logins
CREATE TABLE sessions (
  id         TEXT PRIMARY KEY,
  user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  expires_at TIMESTAMPTZ NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX sessions_user ON sessions (user_id);
CREATE INDEX sessions_expiry ON sessions (expires_at);

CREATE TABLE wallet_challenges (
  id         TEXT PRIMARY KEY,
  user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  address    TEXT NOT NULL,
  message    TEXT NOT NULL,
  nonce      TEXT NOT NULL UNIQUE,
  expires_at TIMESTAMPTZ NOT NULL,
  used_at    TIMESTAMPTZ
);
CREATE INDEX wallet_challenges_user ON wallet_challenges (user_id);

CREATE TABLE installations (
  id            BIGINT PRIMARY KEY,
  account_login TEXT NOT NULL,
  account_type  TEXT NOT NULL,
  suspended_at  TIMESTAMPTZ,
  created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE repos (
  id              BIGINT PRIMARY KEY,
  installation_id BIGINT NOT NULL REFERENCES installations(id),
  full_name       TEXT NOT NULL,
  is_private      BOOLEAN NOT NULL
);
CREATE INDEX repos_installation ON repos (installation_id);

CREATE TABLE funding_intents (
  id            TEXT PRIMARY KEY,
  repo_id       BIGINT NOT NULL,
  issue_number  INTEGER NOT NULL,
  requested_by  BIGINT NOT NULL,
  token         TEXT NOT NULL,
  amount        NUMERIC(39,0) NOT NULL CHECK (amount > 0),
  duration_days INTEGER NOT NULL CHECK (duration_days > 0),
  comment_id    BIGINT NOT NULL,
  expires_at    TIMESTAMPTZ NOT NULL,
  used_at       TIMESTAMPTZ,
  created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX funding_intents_issue ON funding_intents (repo_id, issue_number);

CREATE TABLE bounties (
  id                   BIGINT PRIMARY KEY,
  repo_id              BIGINT NOT NULL REFERENCES repos(id),
  issue_number         INTEGER NOT NULL,
  issue_title          TEXT NOT NULL DEFAULT '',
  issue_url            TEXT NOT NULL,
  criteria             TEXT[] NOT NULL DEFAULT '{}',
  maintainer           TEXT NOT NULL,
  maintainer_github_id BIGINT,
  token                TEXT NOT NULL,
  amount               NUMERIC(39,0) NOT NULL CHECK (amount > 0),
  fee_bps              INTEGER NOT NULL CHECK (fee_bps BETWEEN 0 AND 1000),
  deadline             TIMESTAMPTZ NOT NULL,
  relayer_allowed      BOOLEAN NOT NULL,
  status               TEXT NOT NULL CHECK (status IN ('OPEN','RELEASED','REFUNDED','DISPUTED','RESOLVED')),
  winner_address       TEXT,
  winner_github_id     BIGINT,
  fund_tx              TEXT NOT NULL,
  close_tx             TEXT,
  card_comment_id      BIGINT,
  last_event_id        TEXT,
  created_at           TIMESTAMPTZ NOT NULL,
  updated_at           TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX bounties_issue ON bounties (repo_id, issue_number);
CREATE INDEX bounties_status ON bounties (status);
CREATE INDEX bounties_maintainer ON bounties (maintainer);

CREATE TABLE pull_requests (
  id               BIGINT PRIMARY KEY,
  number           INTEGER NOT NULL,
  author_github_id BIGINT NOT NULL,
  author_login     TEXT NOT NULL,
  head_sha         TEXT NOT NULL,
  state            TEXT NOT NULL CHECK (state IN ('open','closed','merged')),
  merged_at        TIMESTAMPTZ,
  diff_hash        TEXT
);

-- one PR can close several bountied issues: one link per (PR, bounty)
CREATE TABLE pull_request_bounties (
  pr_id     BIGINT NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE,
  bounty_id BIGINT NOT NULL REFERENCES bounties(id),
  PRIMARY KEY (pr_id, bounty_id)
);
CREATE INDEX pull_request_bounties_bounty ON pull_request_bounties (bounty_id);

CREATE TABLE ai_reviews (
  id            TEXT PRIMARY KEY,
  pr_id         BIGINT NOT NULL REFERENCES pull_requests(id) ON DELETE CASCADE,
  head_sha      TEXT NOT NULL,
  model         TEXT NOT NULL,
  verdict       TEXT NOT NULL,
  score         INTEGER NOT NULL,
  result        JSONB NOT NULL,
  flags         TEXT[] NOT NULL DEFAULT '{}',
  input_tokens  INTEGER NOT NULL,
  output_tokens INTEGER NOT NULL,
  comment_id    BIGINT,
  created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE (pr_id, head_sha)
);

CREATE TABLE payouts (
  id               TEXT PRIMARY KEY,
  bounty_id        BIGINT NOT NULL REFERENCES bounties(id),
  pr_number        INTEGER NOT NULL,
  winner_github_id BIGINT NOT NULL,
  winner_address   TEXT,
  status           TEXT NOT NULL CHECK (status IN ('PENDING_WALLET','QUEUED','SUBMITTED','CONFIRMED','FAILED','AWAITING_MAINTAINER')),
  idempotency_key  TEXT NOT NULL UNIQUE,
  tx_hash          TEXT,
  attempts         INTEGER NOT NULL DEFAULT 0,
  last_error       TEXT,
  created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX payouts_status ON payouts (status);
CREATE INDEX payouts_winner ON payouts (winner_github_id);

CREATE TABLE disputes (
  id                  TEXT PRIMARY KEY,
  bounty_id           BIGINT NOT NULL REFERENCES bounties(id),
  opened_by_github_id BIGINT NOT NULL,
  reason              TEXT NOT NULL,
  evidence            JSONB NOT NULL DEFAULT '{}',
  status              TEXT NOT NULL CHECK (status IN ('requested','opened_onchain','resolved','rejected')),
  resolution_note     TEXT,
  created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX disputes_status ON disputes (status);

CREATE TABLE webhook_deliveries (
  id           TEXT PRIMARY KEY,
  event        TEXT NOT NULL,
  received_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
  processed_at TIMESTAMPTZ,
  error        TEXT
);

CREATE TABLE indexer_cursors (
  id         TEXT PRIMARY KEY,
  cursor     TEXT,
  ledger     INTEGER NOT NULL,
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE audit_log (
  id         TEXT PRIMARY KEY,
  actor      TEXT NOT NULL,
  action     TEXT NOT NULL,
  target     TEXT NOT NULL,
  data       JSONB NOT NULL DEFAULT '{}',
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX audit_log_lookup ON audit_log (action, target);

-- Durable job queue (replaces Redis/BullMQ): claimed with FOR UPDATE SKIP LOCKED, retried with backoff.
CREATE TABLE jobs (
  id           BIGSERIAL PRIMARY KEY,
  kind         TEXT NOT NULL,
  payload      JSONB NOT NULL,
  status       TEXT NOT NULL DEFAULT 'queued' CHECK (status IN ('queued','running','done','dead')),
  run_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
  attempts     INTEGER NOT NULL DEFAULT 0,
  max_attempts INTEGER NOT NULL DEFAULT 5,
  dedupe_key   TEXT,
  last_error   TEXT,
  locked_until TIMESTAMPTZ,
  created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX jobs_ready ON jobs (run_at) WHERE status IN ('queued','running');
CREATE UNIQUE INDEX jobs_dedupe_queued ON jobs (dedupe_key) WHERE status = 'queued' AND dedupe_key IS NOT NULL;
