-- Per-account "drain first" flag. When true, the account is preferred during
-- dispatch until its inflight slots are full or it enters cooldown; then
-- requests fall through to normal round-robin. Used for limited-quota
-- (promotional / trial) accounts that should be exhausted before the main pool.
ALTER TABLE accounts ADD COLUMN drain_first INTEGER NOT NULL DEFAULT 0
    CHECK (drain_first IN (0, 1));

CREATE INDEX idx_accounts_drain_first ON accounts(drain_first) WHERE drain_first = 1;
