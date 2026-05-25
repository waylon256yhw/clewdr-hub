-- Step 5: introduce `api_key` as a third value of accounts.auth_source.
--
-- API-key accounts represent pay-as-you-go Anthropic-compatible endpoints
-- (official api.anthropic.com or custom endpoints such as AWS) keyed by
-- an x-api-key bearer rather than a session cookie or OAuth token. Per
-- the PRD (docs/custom-anthropic-api-key-prd-2026-05-25.md) and the
-- 2026-05-25 decisions, three new columns hold the credential:
--
--   api_key_base_url      — free-form upstream URL (NOT a UNIQUE column;
--                           a single endpoint may host multiple keys).
--   api_key_secret        — the API key itself.
--   api_key_extra_headers — optional JSON object of extra HTTP headers
--                           (e.g. anthropic-workspace-id). Values are
--                           treated as secrets by the export/import and
--                           Debug paths.
--
-- All three are TEXT — API keys and base URLs are ASCII strings, and
-- mixing BLOB with String at the Rust layer is brittle.
--
-- SQLite cannot ALTER an existing CHECK constraint, so the table is
-- rebuilt via the rename-and-rebuild trick (same shape as
-- 20260421000003_drop_hybrid_auth_source.sql).
--
-- New mutex CHECK arms:
--   cookie   : cookie_blob NOT NULL, every oauth_* and api_key_* NULL
--   oauth    : cookie_blob NULL, oauth_access/refresh/expires NOT NULL,
--              api_key_* NULL
--   api_key  : cookie_blob NULL, oauth_* NULL, api_key_secret NOT NULL,
--              api_key_base_url NOT NULL. api_key_extra_headers is
--              free to be NULL or a JSON object.
--
-- The cookie/oauth arms explicitly require `api_key_extra_headers IS NULL`
-- so a row switching back from api_key cannot silently leave stale
-- secret-bearing headers attached.

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
        auth_source IN ('cookie', 'oauth', 'api_key')
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
    last_failure_json TEXT,
    rate_limit_tier TEXT,
    subscription_created_at TEXT,
    billing_type TEXT,
    api_key_base_url TEXT,
    api_key_secret TEXT,
    api_key_extra_headers TEXT,
    CHECK (
        (auth_source = 'cookie'
            AND cookie_blob IS NOT NULL
            AND oauth_access_token IS NULL
            AND oauth_refresh_token IS NULL
            AND oauth_expires_at IS NULL
            AND api_key_base_url IS NULL
            AND api_key_secret IS NULL
            AND api_key_extra_headers IS NULL)
        OR
        (auth_source = 'oauth'
            AND cookie_blob IS NULL
            AND oauth_access_token IS NOT NULL
            AND oauth_refresh_token IS NOT NULL
            AND oauth_expires_at IS NOT NULL
            AND api_key_base_url IS NULL
            AND api_key_secret IS NULL
            AND api_key_extra_headers IS NULL)
        OR
        (auth_source = 'api_key'
            AND cookie_blob IS NULL
            AND oauth_access_token IS NULL
            AND oauth_refresh_token IS NULL
            AND oauth_expires_at IS NULL
            AND api_key_secret IS NOT NULL
            AND api_key_base_url IS NOT NULL)
    )
);

INSERT INTO accounts_new (
    id, name, rr_order, max_slots, status, auth_source, cookie_blob,
    oauth_access_token, oauth_refresh_token, oauth_expires_at, organization_uuid,
    last_refresh_at, last_used_at, last_error, created_at, updated_at,
    invalid_reason, email, account_type, drain_first, proxy_id,
    last_failure_json, rate_limit_tier, subscription_created_at, billing_type
)
SELECT
    id, name, rr_order, max_slots, status, auth_source, cookie_blob,
    oauth_access_token, oauth_refresh_token, oauth_expires_at, organization_uuid,
    last_refresh_at, last_used_at, last_error, created_at, updated_at,
    invalid_reason, email, account_type, drain_first, proxy_id,
    last_failure_json, rate_limit_tier, subscription_created_at, billing_type
FROM accounts;

DROP TABLE accounts;
ALTER TABLE accounts_new RENAME TO accounts;

CREATE INDEX idx_accounts_status_rr ON accounts(status, rr_order);
CREATE INDEX idx_accounts_drain_first ON accounts(drain_first) WHERE drain_first = 1;
CREATE INDEX idx_accounts_proxy_id ON accounts(proxy_id);

PRAGMA foreign_keys=ON;
