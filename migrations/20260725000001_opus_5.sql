-- Claude Opus 5 launch (2026-07-24): pricing seed.
--
-- Standard pricing remains $5 / $25 per 1M tokens (input / output), matching
-- Claude Opus 4.8. The builtin models row is handled by seed_models() /
-- DEFAULT_MODELS in src/db/mod.rs on startup, so this migration only adds the
-- canonical pricing row used by usage accounting.

INSERT OR REPLACE INTO model_pricing
    (pricing_key, display_name, input_nanousd_per_token, output_nanousd_per_token)
VALUES ('claude-opus-5', 'Claude Opus 5', 5000, 25000);
