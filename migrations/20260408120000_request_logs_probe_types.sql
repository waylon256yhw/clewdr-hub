PRAGMA foreign_keys = OFF;

-- Remove legacy count_tokens rows (production has none; dev DBs may).
DELETE FROM request_logs WHERE request_type = 'count_tokens';

CREATE TABLE request_logs_new (
    id INTEGER PRIMARY KEY,
    request_id TEXT NOT NULL UNIQUE,
    request_type TEXT NOT NULL CHECK (request_type IN ('messages', 'probe_cookie', 'probe_oauth')),
    user_id INTEGER REFERENCES users(id) ON DELETE SET NULL,
    api_key_id INTEGER REFERENCES api_keys(id) ON DELETE SET NULL,
    model_raw TEXT,
    model_normalized TEXT,
    stream INTEGER NOT NULL DEFAULT 1 CHECK (stream IN (0, 1)),
    started_at TEXT NOT NULL,
    completed_at TEXT,
    duration_ms INTEGER,
    status TEXT NOT NULL CHECK (
        status IN (
            'ok', 'auth_rejected', 'quota_rejected',
            'user_concurrency_rejected', 'rpm_rejected',
            'no_account_available', 'upstream_error', 'client_abort',
            'internal_error'
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
    rate_limit_reset_at TEXT,
    account_id INTEGER REFERENCES accounts(id) ON DELETE SET NULL,
    cache_creation_tokens INTEGER,
    cache_read_tokens INTEGER,
    ttft_ms INTEGER,
    response_body TEXT
);

INSERT INTO request_logs_new (
    id, request_id, request_type, user_id, api_key_id,
    model_raw, model_normalized, stream,
    started_at, completed_at, duration_ms, status,
    http_status, upstream_request_id,
    input_tokens, output_tokens,
    priced_input_nanousd_per_token, priced_output_nanousd_per_token,
    cost_nanousd, error_code, error_message, rate_limit_reset_at,
    account_id, cache_creation_tokens, cache_read_tokens, ttft_ms
)
SELECT
    id, request_id, request_type, user_id, api_key_id,
    model_raw, model_normalized, stream,
    started_at, completed_at, duration_ms, status,
    http_status, upstream_request_id,
    input_tokens, output_tokens,
    priced_input_nanousd_per_token, priced_output_nanousd_per_token,
    cost_nanousd, error_code, error_message, rate_limit_reset_at,
    account_id, cache_creation_tokens, cache_read_tokens, ttft_ms
FROM request_logs;

DROP TABLE request_logs;
ALTER TABLE request_logs_new RENAME TO request_logs;

CREATE INDEX idx_request_logs_user_started ON request_logs(user_id, started_at DESC);
CREATE INDEX idx_request_logs_status_started ON request_logs(status, started_at DESC);
CREATE INDEX idx_request_logs_account_started ON request_logs(account_id, started_at DESC);

PRAGMA foreign_keys = ON;
