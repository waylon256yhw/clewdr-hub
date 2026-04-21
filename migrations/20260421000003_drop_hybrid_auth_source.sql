-- Finalize credential-replacement semantics by removing `hybrid` from the
-- accounts.auth_source enum. Every account carries exactly one active
-- credential: either cookie_blob, or a complete OAuth token set
-- (access_token, refresh_token, expires_at).
--
-- Legacy hybrid rows are normalized first, then any rows whose credential
-- columns drifted out of the canonical shape are canonicalized so the
-- post-rebuild mutex CHECK accepts every surviving row.

PRAGMA foreign_keys=OFF;

-- 1. Normalize legacy hybrid rows.
UPDATE accounts
SET cookie_blob = NULL,
    auth_source = 'oauth'
WHERE auth_source = 'hybrid'
  AND oauth_access_token IS NOT NULL
  AND oauth_refresh_token IS NOT NULL
  AND oauth_expires_at IS NOT NULL;

UPDATE accounts
SET oauth_access_token = NULL,
    oauth_refresh_token = NULL,
    oauth_expires_at = NULL,
    last_refresh_at = NULL,
    auth_source = 'cookie'
WHERE auth_source = 'hybrid';

-- 2. Canonicalize every remaining row against the final CHECK shape:
--
--    cookie row:  cookie_blob NOT NULL, every oauth_* column NULL
--    oauth row:   cookie_blob NULL,     oauth_access_token / refresh_token /
--                                        expires_at all NOT NULL
--
-- A naive cleanup that only looked at oauth_access_token or cookie_blob
-- would miss partial-drift shapes such as a cookie row carrying a residual
-- oauth_refresh_token, or an oauth row missing oauth_expires_at. Those
-- would trip the new mutex CHECK during the table rebuild below.

-- 2a. Rows with a complete OAuth token set take the oauth shape and shed
-- any cookie residue, regardless of their declared auth_source.
UPDATE accounts
SET auth_source = 'oauth',
    cookie_blob = NULL
WHERE oauth_access_token IS NOT NULL
  AND oauth_refresh_token IS NOT NULL
  AND oauth_expires_at IS NOT NULL;

-- 2b. Rows with a cookie_blob but an incomplete OAuth token set take the
-- cookie shape and shed whatever oauth columns drifted in.
UPDATE accounts
SET auth_source = 'cookie',
    oauth_access_token = NULL,
    oauth_refresh_token = NULL,
    oauth_expires_at = NULL,
    last_refresh_at = NULL
WHERE cookie_blob IS NOT NULL
  AND NOT (oauth_access_token IS NOT NULL
           AND oauth_refresh_token IS NOT NULL
           AND oauth_expires_at IS NOT NULL);

-- 2c. Rows without a cookie_blob AND without a complete OAuth token set
-- cannot authenticate under either branch of the new CHECK. They were
-- already unusable in production (no credential to present to Anthropic),
-- so they are dropped rather than failing the whole migration.
DELETE FROM accounts
WHERE cookie_blob IS NULL
  AND NOT (oauth_access_token IS NOT NULL
           AND oauth_refresh_token IS NOT NULL
           AND oauth_expires_at IS NOT NULL);

-- 3. Rebuild the accounts table with:
--    a) auth_source CHECK tightened to ('cookie', 'oauth')
--    b) a mutex CHECK on credential columns keyed by auth_source
CREATE TABLE accounts_new (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    rr_order INTEGER NOT NULL UNIQUE,
    max_slots INTEGER NOT NULL DEFAULT 5 CHECK (max_slots > 0),
    status TEXT NOT NULL CHECK (
        status IN ('active', 'cooldown', 'auth_error', 'disabled')
    ) DEFAULT 'active',
    auth_source TEXT NOT NULL CHECK (
        auth_source IN ('cookie', 'oauth')
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
    account_type TEXT,
    drain_first INTEGER NOT NULL DEFAULT 0 CHECK (drain_first IN (0, 1)),
    proxy_id INTEGER REFERENCES proxies(id) ON DELETE SET NULL,
    CHECK (
        (auth_source = 'cookie'
            AND cookie_blob IS NOT NULL
            AND oauth_access_token IS NULL
            AND oauth_refresh_token IS NULL
            AND oauth_expires_at IS NULL)
        OR
        (auth_source = 'oauth'
            AND cookie_blob IS NULL
            AND oauth_access_token IS NOT NULL
            AND oauth_refresh_token IS NOT NULL
            AND oauth_expires_at IS NOT NULL)
    )
);

INSERT INTO accounts_new (
    id, name, rr_order, max_slots, status, auth_source, cookie_blob,
    oauth_access_token, oauth_refresh_token, oauth_expires_at, organization_uuid,
    last_refresh_at, last_used_at, last_error, created_at, updated_at,
    invalid_reason, email, account_type, drain_first, proxy_id
)
SELECT
    id, name, rr_order, max_slots, status, auth_source, cookie_blob,
    oauth_access_token, oauth_refresh_token, oauth_expires_at, organization_uuid,
    last_refresh_at, last_used_at, last_error, created_at, updated_at,
    invalid_reason, email, account_type, drain_first, proxy_id
FROM accounts;

DROP TABLE accounts;
ALTER TABLE accounts_new RENAME TO accounts;

CREATE INDEX idx_accounts_status_rr ON accounts(status, rr_order);
CREATE INDEX idx_accounts_drain_first ON accounts(drain_first) WHERE drain_first = 1;
CREATE INDEX idx_accounts_proxy_id ON accounts(proxy_id);

PRAGMA foreign_keys=ON;
