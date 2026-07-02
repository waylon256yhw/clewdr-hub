-- Claude Sonnet 5 launch: pricing seed.
--
-- Standard pricing is $3 / $15 per 1M tokens (input / output). Anthropic's
-- introductory $2 / $10 rate runs through 2026-08-31 only, so we seed the
-- durable standard rate rather than a time-bounded discount — matching the
-- single-rate-per-model approach used for the Opus 4.8 fast-mode preview.
-- Seeding standard avoids a follow-up migration when the intro window closes
-- and can only over-estimate (never under-charge) during it.
--
-- The builtin models row is handled by seed_models() / DEFAULT_MODELS in
-- src/db/mod.rs on startup (INSERT OR IGNORE), so this migration only touches
-- pricing — mirroring the fable_5 / opus_4_8 seeds.

INSERT OR REPLACE INTO model_pricing
    (pricing_key, display_name, input_nanousd_per_token, output_nanousd_per_token)
VALUES ('claude-sonnet-5', 'Claude Sonnet 5', 3000, 15000);
