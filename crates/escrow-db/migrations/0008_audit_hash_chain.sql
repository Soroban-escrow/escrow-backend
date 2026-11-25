-- Tamper-evident audit log: every row carries seq, prev_hash and hash.
-- hash = sha256(prev_hash || canonical(row)), where canonical(row) is the jsonb text of
-- {seq, id, actor, action, target, data, created_at} with created_at as UTC RFC 3339 with microseconds.
-- jsonb text output sorts keys deterministically, so the encoding is stable.
-- The first row uses prev_hash = 16 zero bytes.
-- Rows that existed before this migration are chained in created_at, id order; they are only
-- protected from this point on.

ALTER TABLE audit_log ADD COLUMN seq BIGINT;
ALTER TABLE audit_log ADD COLUMN prev_hash BYTEA;
ALTER TABLE audit_log ADD COLUMN hash BYTEA;

CREATE FUNCTION audit_canonical(p_seq BIGINT, p_id TEXT, p_actor TEXT, p_action TEXT, p_target TEXT, p_data JSONB, p_created TIMESTAMPTZ)
RETURNS TEXT
LANGUAGE sql
IMMUTABLE
AS $$
  SELECT jsonb_build_object(
    'seq', p_seq,
    'id', p_id,
    'actor', p_actor,
    'action', p_action,
    'target', p_target,
    'data', p_data,
    'created_at', to_char(p_created AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"')
  )::text
$$;

-- Backfill existing rows in order.
DO $$
DECLARE
  r RECORD;
  n BIGINT := 0;
  prev BYTEA := '\x00000000000000000000000000000000'::bytea;
BEGIN
  FOR r IN SELECT id, actor, action, target, data, created_at FROM audit_log ORDER BY created_at, id LOOP
    n := n + 1;
    UPDATE audit_log
       SET seq = n,
           prev_hash = prev,
           hash = sha256(prev || convert_to(audit_canonical(n, r.id, r.actor, r.action, r.target, r.data, r.created_at), 'UTF8'))
     WHERE id = r.id
     RETURNING hash INTO prev;
  END LOOP;
END
$$;

CREATE UNIQUE INDEX audit_log_seq_unique ON audit_log (seq);

-- Assigns seq and hashes on every insert. The advisory lock serialises writers so the chain has no forks.
-- The trigger runs inside the insert's transaction, so existing callers of audit::log need no changes.
CREATE FUNCTION audit_log_chain() RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
  last_seq BIGINT;
  last_hash BYTEA;
BEGIN
  PERFORM pg_advisory_xact_lock(hashtext('audit_log_chain'));
  SELECT seq, hash INTO last_seq, last_hash FROM audit_log ORDER BY seq DESC LIMIT 1;
  NEW.seq := COALESCE(last_seq, 0) + 1;
  NEW.prev_hash := COALESCE(last_hash, '\x00000000000000000000000000000000'::bytea);
  NEW.hash := sha256(NEW.prev_hash || convert_to(audit_canonical(NEW.seq, NEW.id, NEW.actor, NEW.action, NEW.target, NEW.data, NEW.created_at), 'UTF8'));
  RETURN NEW;
END
$$;

CREATE TRIGGER audit_log_chain_insert
  BEFORE INSERT ON audit_log
  FOR EACH ROW EXECUTE FUNCTION audit_log_chain();

ALTER TABLE audit_log ALTER COLUMN seq SET NOT NULL;
ALTER TABLE audit_log ALTER COLUMN prev_hash SET NOT NULL;
ALTER TABLE audit_log ALTER COLUMN hash SET NOT NULL;

-- Walks the chain from p_from and returns the first broken link, or no rows if the chain is intact.
-- Deleting the tail of the table cannot be detected from the chain alone: compare the head
-- (max seq, hash) with a copy exported outside the database (see docs/SECURITY.md).
CREATE FUNCTION audit_verify(p_from BIGINT DEFAULT 1)
RETURNS TABLE (broken_seq BIGINT, broken_id TEXT, reason TEXT)
LANGUAGE plpgsql
STABLE
AS $$
DECLARE
  r RECORD;
  prev BYTEA;
BEGIN
  IF p_from <= 1 THEN
    prev := '\x00000000000000000000000000000000'::bytea;
  ELSE
    SELECT hash INTO prev FROM audit_log WHERE seq = p_from - 1;
  END IF;

  FOR r IN SELECT * FROM audit_log WHERE seq >= p_from ORDER BY seq LOOP
    IF r.prev_hash IS DISTINCT FROM prev THEN
      broken_seq := r.seq; broken_id := r.id; reason := 'prev_hash does not match previous row';
      RETURN NEXT; RETURN;
    END IF;
    IF r.hash IS DISTINCT FROM sha256(r.prev_hash || convert_to(audit_canonical(r.seq, r.id, r.actor, r.action, r.target, r.data, r.created_at), 'UTF8')) THEN
      broken_seq := r.seq; broken_id := r.id; reason := 'hash does not match row contents';
      RETURN NEXT; RETURN;
    END IF;
    prev := r.hash;
  END LOOP;
END
$$;
