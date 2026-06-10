-- Claude Fable 5 launch (2026-06-09): pricing seed.
--
-- Fable 5 costs twice the standard Opus 4.7 / 4.8 rate:
-- $10 per million input tokens and $50 per million output tokens.

INSERT OR REPLACE INTO model_pricing
    (pricing_key, display_name, input_nanousd_per_token, output_nanousd_per_token)
VALUES ('claude-fable-5', 'Claude Fable 5', 10000, 50000);
