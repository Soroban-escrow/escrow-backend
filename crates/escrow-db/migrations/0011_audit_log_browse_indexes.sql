-- #133 — Support indexes for GET /admin/audit.
--
-- `audit_log_lookup (action, target)` already covers the filter-by-action
-- browse, including target-prefix matches when an action is set. These two
-- indexes keep the actor-only and unfiltered browses index-ordered so the
-- keyset cursor over (created_at DESC, id DESC) does not drop into a sort
-- scan as the table grows.

CREATE INDEX IF NOT EXISTS audit_log_created_at ON audit_log (created_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS audit_log_actor      ON audit_log (actor, created_at DESC);
