-- Per-account custom request-body injection for API-key channels.
--
-- Some Anthropic-compatible upstreams (e.g. Pioneer AI's auto-router) require
-- non-standard top-level body parameters that are absent from the Anthropic
-- schema — Pioneer pins its candidate pool with `"models": ["claude-opus-4-7"]`.
-- The typed request struct (CreateMessageParams) has no flatten catch-all, so
-- such a field can neither be carried by the existing api_key_extra_headers
-- feature (HTTP headers, not body) nor round-tripped through the struct.
--
-- api_key_extra_body — optional JSON object, shallow-merged over the outgoing
--                      request body just before send (api_key channels only,
--                      /v1/messages only). Reserved keys (messages/system) are
--                      rejected at write time; see admin validation.
--
-- SQLite cannot ALTER an existing CHECK constraint, so the table is rebuilt via
-- the rename-and-rebuild trick (same shape as 20260702000002_mimicry.sql).
--
-- Mutex CHECK addition:
--   cookie / oauth arms : api_key_extra_body IS NULL — a non-api_key row must
--                         never carry api-key body state (mirrors the existing
--                         api_key_* = NULL rule).
--   api_key arm         : unconstrained (NULL or JSON), same as
--                         api_key_extra_headers.

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
    api_key_extra_body TEXT,
    mimicry_mode TEXT NOT NULL DEFAULT 'none' CHECK (
        mimicry_mode IN ('none', 'third_party')
    ),
    mimicry_config TEXT,
    CHECK (
        (auth_source = 'cookie'
            AND cookie_blob IS NOT NULL
            AND oauth_access_token IS NULL
            AND oauth_refresh_token IS NULL
            AND oauth_expires_at IS NULL
            AND api_key_base_url IS NULL
            AND api_key_secret IS NULL
            AND api_key_extra_headers IS NULL
            AND api_key_extra_body IS NULL
            AND mimicry_mode = 'none'
            AND mimicry_config IS NULL)
        OR
        (auth_source = 'oauth'
            AND cookie_blob IS NULL
            AND oauth_access_token IS NOT NULL
            AND oauth_refresh_token IS NOT NULL
            AND oauth_expires_at IS NOT NULL
            AND api_key_base_url IS NULL
            AND api_key_secret IS NULL
            AND api_key_extra_headers IS NULL
            AND api_key_extra_body IS NULL
            AND mimicry_mode = 'none'
            AND mimicry_config IS NULL)
        OR
        (auth_source = 'api_key'
            AND cookie_blob IS NULL
            AND oauth_access_token IS NULL
            AND oauth_refresh_token IS NULL
            AND oauth_expires_at IS NULL
            AND api_key_secret IS NOT NULL
            AND api_key_base_url IS NOT NULL
            AND (mimicry_config IS NULL OR mimicry_mode = 'third_party'))
    )
);

INSERT INTO accounts_new (
    id, name, rr_order, max_slots, status, auth_source, cookie_blob,
    oauth_access_token, oauth_refresh_token, oauth_expires_at, organization_uuid,
    last_refresh_at, last_used_at, last_error, created_at, updated_at,
    invalid_reason, email, account_type, drain_first, proxy_id,
    last_failure_json, rate_limit_tier, subscription_created_at, billing_type,
    api_key_base_url, api_key_secret, api_key_extra_headers,
    mimicry_mode, mimicry_config
)
SELECT
    id, name, rr_order, max_slots, status, auth_source, cookie_blob,
    oauth_access_token, oauth_refresh_token, oauth_expires_at, organization_uuid,
    last_refresh_at, last_used_at, last_error, created_at, updated_at,
    invalid_reason, email, account_type, drain_first, proxy_id,
    last_failure_json, rate_limit_tier, subscription_created_at, billing_type,
    api_key_base_url, api_key_secret, api_key_extra_headers,
    mimicry_mode, mimicry_config
FROM accounts;

DROP TABLE accounts;
ALTER TABLE accounts_new RENAME TO accounts;

CREATE INDEX idx_accounts_status_rr ON accounts(status, rr_order);
CREATE INDEX idx_accounts_drain_first ON accounts(drain_first) WHERE drain_first = 1;
CREATE INDEX idx_accounts_proxy_id ON accounts(proxy_id);

PRAGMA foreign_keys=ON;
