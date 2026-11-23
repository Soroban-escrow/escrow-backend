-- Repositories removed from an installation keep their row (bounties reference it) but are
-- marked so new bounties cannot be created on them.
ALTER TABLE repos ADD COLUMN removed_at TIMESTAMPTZ;
