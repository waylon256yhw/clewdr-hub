-- Phase 5.6: Cleanup account status model
-- Remove dead cooldown_until/cooldown_reason columns
-- Normalize any stale cooldown/auth_error status to active

UPDATE accounts SET status = 'active' WHERE status IN ('cooldown', 'auth_error');

ALTER TABLE accounts DROP COLUMN cooldown_until;
ALTER TABLE accounts DROP COLUMN cooldown_reason;
