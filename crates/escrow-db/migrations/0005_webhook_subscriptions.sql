-- Outbound signed webhooks (see escrow-engine/src/webhook_guard.rs for the URL guard).
-- A subscription's secret is stored as-is, because HMAC signing needs it; it is shown once at creation
-- and rotated by replacing it. Deliveries are the audit trail: attempts, response code and duration.
-- NOTE: the inbound GitHub deliveries live in `webhook_deliveries` (0001); the outbound ones are named
-- `webhook_subscription_deliveries` here to avoid clashing with that table.
CREATE TABLE webhook_subscriptions (
  id          TEXT PRIMARY KEY,
  url         TEXT NOT NULL,
  secret      TEXT NOT NULL,
  events      TEXT[] NOT NULL DEFAULT '{}',
  enabled     BOOLEAN NOT NULL DEFAULT TRUE,
  created_by  BIGINT,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE webhook_subscription_deliveries (
  id              BIGSERIAL PRIMARY KEY,
  subscription_id TEXT NOT NULL REFERENCES webhook_subscriptions (id) ON DELETE CASCADE,
  event_id        TEXT NOT NULL,
  event_type      TEXT NOT NULL,
  payload         JSONB NOT NULL,
  status          TEXT NOT NULL CHECK (status IN ('queued', 'delivered', 'failed', 'dead')),
  attempts        INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
  response_code   INTEGER,
  duration_ms     INTEGER,
  last_error      TEXT,
  run_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
  created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  -- one delivery per subscription per event: a re-delivered event never sends twice
  UNIQUE (subscription_id, event_id)
);

CREATE INDEX webhook_subscription_deliveries_due ON webhook_subscription_deliveries (run_at) WHERE status = 'queued';