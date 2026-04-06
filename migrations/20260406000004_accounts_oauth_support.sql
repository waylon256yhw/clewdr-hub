PRAGMA foreign_keys=OFF;

CREATE TABLE accounts_new (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    rr_order INTEGER NOT NULL UNIQUE,
    max_slots INTEGER NOT NULL DEFAULT 5 CHECK (max_slots > 0),
    status TEXT NOT NULL CHECK (
        status IN ('active', 'cooldown', 'auth_error', 'disabled')
    ) DEFAULT 'active',
    auth_source TEXT NOT NULL CHECK (
        auth_source IN ('cookie', 'oauth', 'hybrid')
    ) DEFAULT 'cookie',
    cookie_blob BLOB,
    oauth_access_token BLOB,
    oauth_refresh_token BLOB,
    oauth_expires_at TEXT,
    organization_uuid TEXT,
    last_refresh_at TEXT,
    last_used_at TEXT,
    last_error TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    invalid_reason TEXT,
    email TEXT,
    account_type TEXT
);

INSERT INTO accounts_new (
    id, name, rr_order, max_slots, status, auth_source, cookie_blob,
    oauth_access_token, oauth_refresh_token, oauth_expires_at, organization_uuid,
    last_refresh_at, last_used_at, last_error, created_at, updated_at,
    invalid_reason, email, account_type
)
SELECT
    id,
    name,
    rr_order,
    max_slots,
    status,
    CASE
        WHEN cookie_blob IS NOT NULL
             AND oauth_access_token IS NOT NULL
             AND oauth_refresh_token IS NOT NULL THEN 'hybrid'
        WHEN cookie_blob IS NOT NULL THEN 'cookie'
        ELSE 'oauth'
    END,
    cookie_blob,
    oauth_access_token,
    oauth_refresh_token,
    oauth_expires_at,
    organization_uuid,
    last_refresh_at,
    last_used_at,
    last_error,
    created_at,
    updated_at,
    invalid_reason,
    email,
    account_type
FROM accounts;

DROP TABLE accounts;
ALTER TABLE accounts_new RENAME TO accounts;

CREATE INDEX idx_accounts_status_rr ON accounts(status, rr_order);

PRAGMA foreign_keys=ON;
