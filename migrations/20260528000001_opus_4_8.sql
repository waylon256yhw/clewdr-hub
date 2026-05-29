-- Claude Opus 4.8 launch (2026-05-28): pricing seed + retire haiku-3-5.
--
-- Standard pricing matches Opus 4.7 at $5 / $25 per 1M tokens. The fast-mode
-- research preview ($10 / $50) is not modeled separately — billing falls back
-- to a single rate per model.
--
-- Skipping the admin->builtin reclaim and retroactive cost correction we ran
-- for 4.7: this migration ships within hours of GA, so the window for a user
-- to pre-add the model via admin UI and accumulate mis-priced rows is
-- negligible for this deployment's scale.
--
-- Also drop claude-haiku-3-5 from the builtin model list. Anthropic retired
-- it on 2026-02-19 (requests now 404). Admin-added rows for it (source =
-- 'admin') are left alone so operators can decide whether to keep them.
-- KNOWN_ALIASES in src/billing.rs intentionally still references
-- claude-haiku-3-5 / claude-3-5-haiku / claude-3-haiku so historical
-- request_logs continue to normalize against their original pricing key
-- rather than falling back to the most-expensive default.

INSERT OR REPLACE INTO model_pricing
    (pricing_key, display_name, input_nanousd_per_token, output_nanousd_per_token)
VALUES ('claude-opus-4-8', 'Claude Opus 4.8', 5000, 25000);

DELETE FROM models
WHERE source = 'builtin' AND model_id = 'claude-haiku-3-5';
