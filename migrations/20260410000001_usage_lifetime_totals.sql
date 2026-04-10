CREATE TABLE usage_lifetime_totals (
    user_id INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    request_count INTEGER NOT NULL DEFAULT 0,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    cost_nanousd INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

INSERT INTO usage_lifetime_totals (
    user_id,
    request_count,
    input_tokens,
    output_tokens,
    cache_creation_tokens,
    cache_read_tokens,
    cost_nanousd,
    updated_at
)
SELECT
    user_id,
    SUM(request_count) AS request_count,
    SUM(input_tokens) AS input_tokens,
    SUM(output_tokens) AS output_tokens,
    SUM(cache_creation_tokens) AS cache_creation_tokens,
    SUM(cache_read_tokens) AS cache_read_tokens,
    SUM(cost_nanousd) AS cost_nanousd,
    CURRENT_TIMESTAMP
FROM usage_rollups
WHERE period_type = 'month'
GROUP BY user_id;

INSERT OR IGNORE INTO usage_lifetime_totals (
    user_id,
    request_count,
    input_tokens,
    output_tokens,
    cache_creation_tokens,
    cache_read_tokens,
    cost_nanousd,
    updated_at
)
SELECT
    user_id,
    COUNT(*) AS request_count,
    COALESCE(SUM(input_tokens), 0) AS input_tokens,
    COALESCE(SUM(output_tokens), 0) AS output_tokens,
    COALESCE(SUM(cache_creation_tokens), 0) AS cache_creation_tokens,
    COALESCE(SUM(cache_read_tokens), 0) AS cache_read_tokens,
    COALESCE(SUM(cost_nanousd), 0) AS cost_nanousd,
    CURRENT_TIMESTAMP
FROM request_logs
WHERE user_id IS NOT NULL
  AND request_type = 'messages'
  AND status = 'ok'
GROUP BY user_id;
