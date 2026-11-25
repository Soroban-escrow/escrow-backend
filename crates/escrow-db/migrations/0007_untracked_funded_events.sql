-- Store Funded events for repositories that are not tracked yet.
-- When the repository is later installed, these events can be applied.

CREATE TABLE untracked_funded_events (
    id              TEXT PRIMARY KEY,
    event_id        TEXT NOT NULL,
    bounty_id       BIGINT NOT NULL,
    repo_id         BIGINT NOT NULL,
    issue_number    INTEGER NOT NULL,
    maintainer      TEXT NOT NULL,
    token           TEXT NOT NULL,
    amount          NUMERIC(39,0) NOT NULL CHECK (amount > 0),
    fee_bps         INTEGER NOT NULL CHECK (fee_bps BETWEEN 0 AND 1000),
    deadline        TIMESTAMPTZ NOT NULL,
    relayer_allowed BOOLEAN NOT NULL,
    ledger          INTEGER NOT NULL,
    tx_hash         TEXT NOT NULL,
    closed_at       TIMESTAMPTZ NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX untracked_funded_events_repo ON untracked_funded_events (repo_id);
CREATE INDEX untracked_funded_events_bounty ON untracked_funded_events (bounty_id);