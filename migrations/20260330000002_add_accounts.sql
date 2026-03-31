-- Phase 3: Add accounts table and request_logs.account_id

CREATE TABLE accounts (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    rr_order INTEGER NOT NULL UNIQUE,
    max_slots INTEGER NOT NULL DEFAULT 5 CHECK (max_slots > 0),
    status TEXT NOT NULL CHECK (
        status IN ('active', 'cooldown', 'auth_error', 'disabled')
    ) DEFAULT 'active',
    cookie_blob BLOB NOT NULL,
    oauth_access_token BLOB,
    oauth_refresh_token BLOB,
    oauth_expires_at TEXT,
    organization_uuid TEXT,
    cooldown_until TEXT,
    cooldown_reason TEXT,
    last_refresh_at TEXT,
    last_used_at TEXT,
    last_error TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

ALTER TABLE request_logs ADD COLUMN account_id INTEGER REFERENCES accounts(id) ON DELETE SET NULL;

CREATE INDEX idx_accounts_status_rr ON accounts(status, rr_order);
CREATE INDEX idx_request_logs_account_started ON request_logs(account_id, started_at DESC);
