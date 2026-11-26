-- Issue #107: track the one-shot "this payout needs attention" comment on the PR so the same
-- notification is not posted on every pass through a FAILED payout. Nullable: the column is set
-- the first time the comment is claimed (atomically, via an UPDATE guarded on IS NULL), and
-- stays NULL forever otherwise. Existing FAILED rows at migration time are not back-notified:
-- their state is already visible to the operator through the `payouts_failed` alert.

ALTER TABLE payouts ADD COLUMN failure_notified_at TIMESTAMPTZ;
