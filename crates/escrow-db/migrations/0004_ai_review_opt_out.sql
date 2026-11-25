-- Per-repository AI review switch. A missing row (or TRUE) means reviews are on.
ALTER TABLE repo_settings ADD COLUMN ai_reviews BOOLEAN NOT NULL DEFAULT TRUE;
