-- Phase 1: Initial schema
-- PRAGMAs are set via SqliteConnectOptions, not here.

-- Policy templates
CREATE TABLE policies (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    max_concurrent INTEGER NOT NULL CHECK (max_concurrent > 0),
    rpm_limit INTEGER NOT NULL CHECK (rpm_limit > 0),
    weekly_budget_nanousd INTEGER NOT NULL CHECK (weekly_budget_nanousd >= 0),
    monthly_budget_nanousd INTEGER NOT NULL CHECK (monthly_budget_nanousd >= 0),
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Users
CREATE TABLE users (
    id INTEGER PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    display_name TEXT,
    password_hash TEXT,
    role TEXT NOT NULL CHECK (role IN ('admin', 'member')) DEFAULT 'member',
    policy_id INTEGER NOT NULL REFERENCES policies(id),
    disabled_at TEXT,
    last_seen_at TEXT,
    notes TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CHECK (role != 'admin' OR password_hash IS NOT NULL)
);

-- API keys (per-user, multiple keys allowed)
CREATE TABLE api_keys (
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    label TEXT,
    lookup_key TEXT NOT NULL UNIQUE,
    key_hash BLOB NOT NULL UNIQUE,
    disabled_at TEXT,
    expires_at TEXT,
    last_used_at TEXT,
    last_used_ip TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Model pricing (nanousd per token for precision)
CREATE TABLE model_pricing (
    id INTEGER PRIMARY KEY,
    pricing_key TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    input_nanousd_per_token INTEGER NOT NULL CHECK (input_nanousd_per_token >= 0),
    output_nanousd_per_token INTEGER NOT NULL CHECK (output_nanousd_per_token >= 0),
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Request logs (price snapshot embedded, history unaffected by price updates)
-- account_id deferred to Phase 3 when accounts table is added
CREATE TABLE request_logs (
    id INTEGER PRIMARY KEY,
    request_id TEXT NOT NULL UNIQUE,
    request_type TEXT NOT NULL CHECK (request_type IN ('messages', 'count_tokens')),
    user_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    api_key_id INTEGER REFERENCES api_keys(id) ON DELETE SET NULL,
    model_raw TEXT NOT NULL,
    model_normalized TEXT,
    stream INTEGER NOT NULL DEFAULT 1 CHECK (stream IN (0, 1)),
    started_at TEXT NOT NULL,
    completed_at TEXT,
    duration_ms INTEGER,
    status TEXT NOT NULL CHECK (
        status IN (
            'ok', 'auth_rejected', 'quota_rejected',
            'user_concurrency_rejected', 'rpm_rejected',
            'no_account_available', 'upstream_error', 'client_abort'
        )
    ),
    http_status INTEGER,
    upstream_request_id TEXT,
    input_tokens INTEGER,
    output_tokens INTEGER,
    priced_input_nanousd_per_token INTEGER,
    priced_output_nanousd_per_token INTEGER,
    cost_nanousd INTEGER NOT NULL DEFAULT 0,
    error_code TEXT,
    error_message TEXT,
    rate_limit_reset_at TEXT
);

-- Usage rollups (UPSERT updated)
CREATE TABLE usage_rollups (
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    period_type TEXT NOT NULL CHECK (period_type IN ('week', 'month')),
    period_start TEXT NOT NULL,
    period_end TEXT NOT NULL,
    request_count INTEGER NOT NULL DEFAULT 0,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cost_nanousd INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (user_id, period_type, period_start)
);

-- Settings (KV store for runtime-mutable config)
CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Indexes
CREATE INDEX idx_users_policy_id ON users(policy_id);
CREATE INDEX idx_api_keys_user_id ON api_keys(user_id);
CREATE INDEX idx_request_logs_user_started ON request_logs(user_id, started_at DESC);
CREATE INDEX idx_request_logs_status_started ON request_logs(status, started_at DESC);
CREATE INDEX idx_usage_rollups_user_period ON usage_rollups(user_id, period_type, period_start DESC);

-- Seed: default policy
INSERT OR IGNORE INTO policies (name, max_concurrent, rpm_limit, weekly_budget_nanousd, monthly_budget_nanousd)
VALUES ('default', 5, 30, 50000000000, 150000000000);

-- Seed: model pricing
INSERT OR IGNORE INTO model_pricing (pricing_key, display_name, input_nanousd_per_token, output_nanousd_per_token)
VALUES
    ('claude-opus-4', 'Claude Opus 4', 15000, 75000),
    ('claude-sonnet-4', 'Claude Sonnet 4', 3000, 15000),
    ('claude-haiku-3.5', 'Claude Haiku 3.5', 800, 4000);
