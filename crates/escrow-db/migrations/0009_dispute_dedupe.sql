-- At most one active dispute (requested or opened_onchain) per bounty.
CREATE UNIQUE INDEX disputes_one_open_per_bounty ON disputes (bounty_id) WHERE status IN ('requested', 'opened_onchain');