-- Phase 5.5: Account runtime state unification
-- Adds invalid_reason to accounts and creates account_runtime_state table

ALTER TABLE accounts ADD COLUMN invalid_reason TEXT;

CREATE TABLE account_runtime_state (
    account_id INTEGER PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,

    -- Cooldown reset time (epoch secs), NULL = not in cooldown
    reset_time INTEGER,

    -- 1M support tri-state: NULL = unknown, 0 = false, 1 = true
    supports_claude_1m_sonnet INTEGER,
    supports_claude_1m_opus INTEGER,
    count_tokens_allowed INTEGER,

    -- Window reset boundaries (epoch secs)
    session_resets_at INTEGER,
    weekly_resets_at INTEGER,
    weekly_sonnet_resets_at INTEGER,
    weekly_opus_resets_at INTEGER,
    resets_last_checked_at INTEGER,

    -- Window tracking tri-state: NULL = unknown, 0 = no limit, 1 = tracked
    session_has_reset INTEGER,
    weekly_has_reset INTEGER,
    weekly_sonnet_has_reset INTEGER,
    weekly_opus_has_reset INTEGER,

    -- 5 buckets x 6 counters = 30 usage columns
    -- Session (5h window)
    session_total_input INTEGER NOT NULL DEFAULT 0,
    session_total_output INTEGER NOT NULL DEFAULT 0,
    session_sonnet_input INTEGER NOT NULL DEFAULT 0,
    session_sonnet_output INTEGER NOT NULL DEFAULT 0,
    session_opus_input INTEGER NOT NULL DEFAULT 0,
    session_opus_output INTEGER NOT NULL DEFAULT 0,

    -- Weekly (7d overall)
    weekly_total_input INTEGER NOT NULL DEFAULT 0,
    weekly_total_output INTEGER NOT NULL DEFAULT 0,
    weekly_sonnet_input INTEGER NOT NULL DEFAULT 0,
    weekly_sonnet_output INTEGER NOT NULL DEFAULT 0,
    weekly_opus_input INTEGER NOT NULL DEFAULT 0,
    weekly_opus_output INTEGER NOT NULL DEFAULT 0,

    -- Weekly Sonnet (7d sonnet-only bucket)
    ws_total_input INTEGER NOT NULL DEFAULT 0,
    ws_total_output INTEGER NOT NULL DEFAULT 0,
    ws_sonnet_input INTEGER NOT NULL DEFAULT 0,
    ws_sonnet_output INTEGER NOT NULL DEFAULT 0,
    ws_opus_input INTEGER NOT NULL DEFAULT 0,
    ws_opus_output INTEGER NOT NULL DEFAULT 0,

    -- Weekly Opus (7d opus-only bucket)
    wo_total_input INTEGER NOT NULL DEFAULT 0,
    wo_total_output INTEGER NOT NULL DEFAULT 0,
    wo_sonnet_input INTEGER NOT NULL DEFAULT 0,
    wo_sonnet_output INTEGER NOT NULL DEFAULT 0,
    wo_opus_input INTEGER NOT NULL DEFAULT 0,
    wo_opus_output INTEGER NOT NULL DEFAULT 0,

    -- Lifetime
    lifetime_total_input INTEGER NOT NULL DEFAULT 0,
    lifetime_total_output INTEGER NOT NULL DEFAULT 0,
    lifetime_sonnet_input INTEGER NOT NULL DEFAULT 0,
    lifetime_sonnet_output INTEGER NOT NULL DEFAULT 0,
    lifetime_opus_input INTEGER NOT NULL DEFAULT 0,
    lifetime_opus_output INTEGER NOT NULL DEFAULT 0,

    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
