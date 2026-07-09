-- Anthropic replaced the fixed `seven_day_opus`/`seven_day_sonnet` usage-probe
-- fields (now always null) with a generic `usage.limits[]` array containing
-- `kind: "weekly_scoped"` entries scoped to an arbitrary model name (e.g.
-- "Fable", not just Opus/Sonnet). This column stores the full parsed list as
-- JSON so any scoped model's weekly limit is visible, not just Opus/Sonnet.
--
-- The existing weekly_opus_*/weekly_sonnet_* columns are untouched and still
-- get backfilled from a matching scoped entry in application code, so
-- existing behavior/UI is unaffected.

ALTER TABLE account_runtime_state ADD COLUMN weekly_scoped_limits_json TEXT;
