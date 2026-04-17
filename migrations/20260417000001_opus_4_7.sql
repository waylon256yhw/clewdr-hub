-- Claude Opus 4.7 launch (2026-04-16): pricing + retire deprecated +
-- retroactive cost correction for rows priced at the fallback rate.
--
-- Context:
--   * Anthropic deprecated claude-opus-4-20250514 / claude-sonnet-4-20250514 on
--     2026-04-14 with retirement on 2026-06-15.
--   * Claude Opus 4.7 launched 2026-04-16. On deployments where users added
--     it as a custom model before this migration, billing.rs::normalize_model()
--     returned None and lookup_prices() fell back to $15/$75 (Opus 4.1 rate),
--     overcharging 3x versus the real $5/$25. Those rows have
--     model_normalized = NULL and priced_*_nanousd_per_token = 15000 / 75000.
--   * model_pricing rows for opus-4-0 / sonnet-4-0 stay so historical logs
--     priced against them still render correctly.

-- 1. Add opus-4-7 to the pricing table.
INSERT OR REPLACE INTO model_pricing
    (pricing_key, display_name, input_nanousd_per_token, output_nanousd_per_token)
VALUES ('claude-opus-4-7', 'Claude Opus 4.7', 5000, 25000);

-- 2. Drop deprecated entries from the built-in model list (leave custom rows).
DELETE FROM models
WHERE source = 'builtin'
  AND model_id IN ('claude-opus-4-0', 'claude-sonnet-4-0');

-- 3. If a user pre-added claude-opus-4-7 via the admin UI, claim it as
--    builtin so seed_models / reset_default_models treat it uniformly.
UPDATE models
SET source = 'builtin',
    sort_order = 5,
    display_name = 'Claude Opus 4.7',
    updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
WHERE model_id = 'claude-opus-4-7' AND source = 'admin';

-- 4. Retroactive billing correction.
--    Formula mirrors BillingUsage::cost_nanousd in src/billing.rs:
--      base_input = input_tokens * input_price
--      cache_create = cache_creation_tokens * input_price * 125 / 100
--      cache_read   = cache_read_tokens     * input_price *  10 / 100
--      output       = output_tokens         * output_price
--    At input=5000 / output=25000.
--
--    Bucket match uses completed_at because persist_terminal_request_log()
--    computes usage_rollups from Utc::now() at completion, not started_at.

CREATE TEMP TABLE _opus47_fix AS
SELECT
    rl.id AS row_id,
    rl.user_id,
    COALESCE(rl.completed_at, rl.started_at) AS bucket_ts,
    rl.cost_nanousd AS old_cost,
    (rl.input_tokens * 5000
     + COALESCE(rl.cache_creation_tokens, 0) * 5000 * 125 / 100
     + COALESCE(rl.cache_read_tokens, 0)     * 5000 *  10 / 100
     + rl.output_tokens * 25000) AS new_cost
FROM request_logs rl
WHERE (lower(rl.model_raw) = 'claude-opus-4-7'
       OR lower(rl.model_raw) GLOB 'claude-opus-4-7-????????')
  AND rl.model_normalized IS NULL
  AND rl.input_tokens IS NOT NULL
  AND rl.output_tokens IS NOT NULL;

-- 4a. Re-stamp request_logs rows.
UPDATE request_logs
SET model_normalized = 'claude-opus-4-7',
    priced_input_nanousd_per_token = 5000,
    priced_output_nanousd_per_token = 25000,
    cost_nanousd = (SELECT new_cost FROM _opus47_fix f WHERE f.row_id = request_logs.id)
WHERE id IN (SELECT row_id FROM _opus47_fix);

-- 4b. Subtract overcharge from weekly rollups.
--     Week bucket mirrors current_week_bounds() in src/billing.rs: Monday 00:00
--     UTC of the week containing bucket_ts. SQLite's strftime('%w') returns
--     Sun=0..Sat=6; shift to Mon=0 by (w+6)%7, then subtract that many days.
UPDATE usage_rollups
SET cost_nanousd = cost_nanousd - COALESCE((
        SELECT SUM(f.old_cost - f.new_cost)
        FROM _opus47_fix f
        WHERE f.user_id = usage_rollups.user_id
          AND date(
                  substr(f.bucket_ts, 1, 10),
                  '-' || ((CAST(strftime('%w', substr(f.bucket_ts, 1, 10)) AS INTEGER) + 6) % 7)
                      || ' days'
              ) || 'T00:00:00Z' = usage_rollups.period_start
    ), 0)
WHERE period_type = 'week'
  AND user_id IN (SELECT DISTINCT user_id FROM _opus47_fix);

-- 4c. Subtract overcharge from monthly rollups.
UPDATE usage_rollups
SET cost_nanousd = cost_nanousd - COALESCE((
        SELECT SUM(f.old_cost - f.new_cost)
        FROM _opus47_fix f
        WHERE f.user_id = usage_rollups.user_id
          AND substr(f.bucket_ts, 1, 7) || '-01T00:00:00Z' = usage_rollups.period_start
    ), 0)
WHERE period_type = 'month'
  AND user_id IN (SELECT DISTINCT user_id FROM _opus47_fix);

-- 4d. Subtract overcharge from lifetime totals.
UPDATE usage_lifetime_totals
SET cost_nanousd = cost_nanousd - COALESCE((
        SELECT SUM(f.old_cost - f.new_cost)
        FROM _opus47_fix f
        WHERE f.user_id = usage_lifetime_totals.user_id
    ), 0)
WHERE user_id IN (SELECT DISTINCT user_id FROM _opus47_fix);

DROP TABLE _opus47_fix;
