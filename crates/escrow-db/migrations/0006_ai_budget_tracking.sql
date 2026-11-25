-- AI budget tracking: cost per review and solvency monitoring

-- Add cost tracking to ai_reviews (cost in micro USD, computed at insert time from tokens and model pricing)
ALTER TABLE ai_reviews
  ADD COLUMN cost_micro_usd BIGINT NOT NULL DEFAULT 0;

-- Index for efficient budget queries (rolling 24h and 30d windows)
CREATE INDEX ai_reviews_created_at ON ai_reviews (created_at DESC);

-- Solvency monitor results: tracks chain vs database inconsistencies
CREATE TABLE solvency_checks (
  id               TEXT PRIMARY KEY,
  token            TEXT NOT NULL,
  ledger_number    INTEGER NOT NULL,
  held_on_chain    NUMERIC(39,0) NOT NULL,
  balance_actual   NUMERIC(39,0) NOT NULL,
  funded_expected  NUMERIC(39,0) NOT NULL,
  released_net_sum NUMERIC(39,0) NOT NULL,
  fees_sum         NUMERIC(39,0) NOT NULL,
  refunded_sum     NUMERIC(39,0) NOT NULL,
  held_expected    NUMERIC(39,0) NOT NULL,
  db_held_sum      NUMERIC(39,0) NOT NULL,
  discrepancy      TEXT,
  severity         TEXT NOT NULL CHECK (severity IN ('ok', 'warning', 'critical')),
  created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX solvency_checks_token_time ON solvency_checks (token, created_at DESC);
CREATE INDEX solvency_checks_severity ON solvency_checks (severity) WHERE severity != 'ok';

-- Event gaps detected during indexing
CREATE TABLE event_gaps (
  id           TEXT PRIMARY KEY,
  gap_start    INTEGER NOT NULL,
  gap_end      INTEGER NOT NULL,
  detected_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
  reconciled_at TIMESTAMPTZ,
  reconcile_error TEXT
);
CREATE INDEX event_gaps_active ON event_gaps (reconciled_at) WHERE reconciled_at IS NULL;
