-- Phase 5.7: Add proxy to runtime settings
INSERT OR IGNORE INTO settings (key, value, updated_at) VALUES ('proxy', '', CURRENT_TIMESTAMP);
