-- A bounty has at most one open dispute. Repeated `/bounty dispute` comments used to stack up rows
-- in the admin queue; the handler now answers with the existing one, and this index makes the
-- rule hold even if two requests race.

-- Databases that already hold duplicates keep the oldest open dispute and close the rest, so the
-- index can be created. The closed rows stay for the audit trail.
UPDATE disputes d
   SET status = 'rejected',
       resolution_note = COALESCE(d.resolution_note, 'Duplicate of an earlier open dispute on the same bounty.'),
       updated_at = now()
 WHERE d.status IN ('requested', 'opened_onchain')
   AND EXISTS (
         SELECT 1
           FROM disputes o
          WHERE o.bounty_id = d.bounty_id
            AND o.status IN ('requested', 'opened_onchain')
            AND (o.created_at, o.id) < (d.created_at, d.id)
       );

CREATE UNIQUE INDEX disputes_one_open_per_bounty ON disputes (bounty_id) WHERE status IN ('requested', 'opened_onchain');
