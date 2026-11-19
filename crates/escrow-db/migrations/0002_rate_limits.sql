-- Fixed-window rate-limit counters shared by every API instance (see escrow-db/src/rate_limits.rs).
-- One row per key per window; old windows are pruned by the worker.
CREATE TABLE rate_limits (
  key          TEXT NOT NULL,
  window_start TIMESTAMPTZ NOT NULL,
  hits         INTEGER NOT NULL CHECK (hits >= 0),
  PRIMARY KEY (key, window_start)
);
CREATE INDEX rate_limits_window ON rate_limits (window_start);
