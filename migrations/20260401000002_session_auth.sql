-- Session auth: add session_version to users, clean up non-proxy keys
ALTER TABLE users ADD COLUMN session_version INTEGER NOT NULL DEFAULT 1;
DELETE FROM api_keys WHERE label IN ('bootstrap', 'web-session');
