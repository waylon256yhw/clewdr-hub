-- Ops Page Overhaul PR-A: data layer for daily rollups + canonical
-- accountability flag on request_logs.
--
-- This migration adds:
--   1. request_logs.model_key      — canonical model identifier used by Ops
--                                     ranking / drill-down (precise filter).
--   2. request_logs.usage_accounted — explicit 0/1 flag set by the writer
--                                     when a row represents billable usage.
--                                     Ops queries filter on this directly
--                                     instead of inferring from status='ok'.
--   3. usage_daily_rollups          — per (user, model, UTC+8 date) bucketed
--                                     pre-aggregate. Allows 7d / 30d Ops
--                                     views to survive log retention pruning.
--   4. usage_daily_rollup_state     — single-row table tracking when
--                                     persistent rollup writes began and
--                                     the earliest backfilled bucket.
--
-- Historical backfill is best-effort: only the still-present request_logs
-- rows can be classified and rolled up, so deployments with short retention
-- will surface a partial 30d window until daily rollups accumulate.

ALTER TABLE request_logs
ADD COLUMN model_key TEXT NOT NULL DEFAULT 'unknown';

ALTER TABLE request_logs
ADD COLUMN usage_accounted INTEGER NOT NULL DEFAULT 0
CHECK (usage_accounted IN (0, 1));

-- Backfill model_key for existing rows.
-- Newly written rows are populated by the application layer via the
-- COALESCE(NULLIF(model_normalized, ''), NULLIF(model_raw, ''), 'unknown')
-- rule held in src/billing.rs.
UPDATE request_logs
SET model_key = COALESCE(
    NULLIF(model_normalized, ''),
    NULLIF(model_raw, ''),
    'unknown'
);

-- Best-effort backfill of usage_accounted for historical rows. New rows
-- are flagged by the writer using the strict three-condition rule:
--     request_type == Messages && opts.usage.is_some() && opts.update_rollups
-- For history we cannot recover opts.update_rollups, so we approximate
-- with "is a messages request that recorded input_tokens". This may
-- include the rare client_abort row that still surfaced upstream usage —
-- the same row already contributed to usage_rollups / usage_lifetime_totals
-- under the old write path, so treating it as accounted keeps daily
-- consistent with the existing aggregates.
UPDATE request_logs
SET usage_accounted = 1
WHERE request_type = 'messages'
  AND input_tokens IS NOT NULL;

CREATE INDEX idx_request_logs_accounted_started
ON request_logs(usage_accounted, started_at DESC)
WHERE usage_accounted = 1;

CREATE INDEX idx_request_logs_model_key
ON request_logs(model_key);

-- Per-day pre-aggregate. UNIQUE key drives UPSERT in the writer.
-- bucket_date_local is the local-date string for the bucket (UTC+8),
-- e.g. '2026-06-02'. It is computed at write time from started_at so
-- that ops queries can index directly on the bucket column.
CREATE TABLE usage_daily_rollups (
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    model_key TEXT NOT NULL,
    bucket_date_local TEXT NOT NULL,
    request_count INTEGER NOT NULL DEFAULT 0,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    cost_nanousd INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (user_id, model_key, bucket_date_local)
);

CREATE INDEX idx_daily_rollups_bucket
ON usage_daily_rollups(bucket_date_local);

CREATE INDEX idx_daily_rollups_user_bucket
ON usage_daily_rollups(user_id, bucket_date_local);

-- Single-row state table. writes_started_at marks the moment the new
-- write path took over; UI uses this to decide whether a comparison
-- window is fully covered. backfill_available_from is the earliest
-- bucket date present after the historical INSERT below, used only as
-- a UI hint ("history backfilled from X").
CREATE TABLE usage_daily_rollup_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    writes_started_at TEXT NOT NULL,
    backfill_available_from TEXT
);

-- Historical backfill of usage_daily_rollups from the still-present
-- request_logs. Uses the just-populated model_key + usage_accounted
-- columns. Empty deployments produce zero rows here and the state row
-- below ends up with backfill_available_from = NULL.
INSERT INTO usage_daily_rollups (
    user_id,
    model_key,
    bucket_date_local,
    request_count,
    input_tokens,
    output_tokens,
    cache_creation_tokens,
    cache_read_tokens,
    cost_nanousd
)
SELECT
    user_id,
    model_key,
    strftime('%Y-%m-%d', datetime(started_at, '+8 hours')),
    COUNT(*),
    COALESCE(SUM(input_tokens), 0),
    COALESCE(SUM(output_tokens), 0),
    COALESCE(SUM(cache_creation_tokens), 0),
    COALESCE(SUM(cache_read_tokens), 0),
    COALESCE(SUM(cost_nanousd), 0)
FROM request_logs
WHERE usage_accounted = 1
  AND user_id IS NOT NULL
GROUP BY
    user_id,
    model_key,
    strftime('%Y-%m-%d', datetime(started_at, '+8 hours'));

-- Seed the state row. MIN over an empty usage_daily_rollups yields NULL,
-- which is exactly what we want: an empty deployment has no historical
-- backfill window to advertise.
INSERT INTO usage_daily_rollup_state (
    id,
    writes_started_at,
    backfill_available_from
)
SELECT
    1,
    CURRENT_TIMESTAMP,
    MIN(bucket_date_local)
FROM usage_daily_rollups;
