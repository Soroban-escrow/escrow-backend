-- Reminders to contributors whose merged PR is waiting for a wallet (see escrow-db/src/payout_reminders.rs).
-- One row per (payout, stage): a restart never repeats a stage.
CREATE TABLE payout_reminders (
  payout_id TEXT NOT NULL REFERENCES payouts(id),
  stage     TEXT NOT NULL CHECK (stage IN ('h1','h24','h72','linked','skipped')),
  sent_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (payout_id, stage)
);

-- Per-repository switch. A missing row means reminders are on.
CREATE TABLE repo_settings (
  repo_id          BIGINT PRIMARY KEY REFERENCES repos(id),
  wallet_reminders BOOLEAN NOT NULL DEFAULT TRUE
);
