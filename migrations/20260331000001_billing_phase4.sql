-- Phase 4: Billing system schema changes + pricing data update

-- Add cache token columns to request_logs
ALTER TABLE request_logs ADD COLUMN cache_creation_tokens INTEGER;
ALTER TABLE request_logs ADD COLUMN cache_read_tokens INTEGER;

-- Add cache token columns to usage_rollups
ALTER TABLE usage_rollups ADD COLUMN cache_creation_tokens INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage_rollups ADD COLUMN cache_read_tokens INTEGER NOT NULL DEFAULT 0;

-- Remove outdated pricing rows (coarse-grained keys from Phase 1)
DELETE FROM model_pricing WHERE pricing_key IN ('claude-opus-4', 'claude-sonnet-4', 'claude-haiku-3.5');

-- Insert correct per-version pricing (nanousd per token)
INSERT OR REPLACE INTO model_pricing (pricing_key, display_name, input_nanousd_per_token, output_nanousd_per_token)
VALUES
    ('claude-opus-4-6',   'Claude Opus 4.6',   5000,  25000),
    ('claude-opus-4-5',   'Claude Opus 4.5',   5000,  25000),
    ('claude-opus-4-1',   'Claude Opus 4.1',   15000, 75000),
    ('claude-opus-4-0',   'Claude Opus 4.0',   15000, 75000),
    ('claude-sonnet-4-6', 'Claude Sonnet 4.6', 3000,  15000),
    ('claude-sonnet-4-5', 'Claude Sonnet 4.5', 3000,  15000),
    ('claude-sonnet-4-0', 'Claude Sonnet 4.0', 3000,  15000),
    ('claude-haiku-4-5',  'Claude Haiku 4.5',  1000,  5000),
    ('claude-haiku-3-5',  'Claude Haiku 3.5',  800,   4000);

-- Default log retention setting
INSERT OR IGNORE INTO settings (key, value) VALUES ('log_retention_days', '7');
