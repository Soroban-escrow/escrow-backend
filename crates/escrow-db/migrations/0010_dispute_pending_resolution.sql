-- Store pending resolution (note and split) separately from confirmed resolution_note.
-- Only promote pending to confirmed once the indexer sees the resolution land on-chain.

ALTER TABLE disputes ADD COLUMN pending_note TEXT;
ALTER TABLE disputes ADD COLUMN pending_winner TEXT;
ALTER TABLE disputes ADD COLUMN pending_winner_bps INT;
