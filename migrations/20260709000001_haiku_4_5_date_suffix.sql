-- Users reported the bare `claude-haiku-4-5` alias 404s upstream — unlike
-- the other builtin models, Anthropic never registered an un-dated alias for
-- Haiku 4.5, so it must be requested as `claude-haiku-4-5-20251001` (see
-- TEST_ACCOUNT_MODEL in src/api/admin/accounts.rs, which already uses the
-- dated id). DEFAULT_MODELS in src/db/mod.rs now seeds the dated id for new
-- installs; this migration repoints existing builtin rows so upgraded
-- deployments pick it up too.
--
-- model_pricing keeps the bare 'claude-haiku-4-5' pricing_key untouched —
-- billing::normalize_model matches alias + date-suffix, so requests against
-- the dated model id still price against the existing row.

UPDATE models
SET model_id = 'claude-haiku-4-5-20251001',
    updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
WHERE source = 'builtin'
  AND model_id = 'claude-haiku-4-5'
  AND NOT EXISTS (
      SELECT 1 FROM models WHERE model_id = 'claude-haiku-4-5-20251001'
  );

-- If a `claude-haiku-4-5-20251001` row already existed (e.g. an admin
-- manually added it before upgrading), the UPDATE above is a no-op due to
-- the UNIQUE constraint guard, which would otherwise leave the broken bare
-- builtin row in place forever. Drop it explicitly in that case.
DELETE FROM models
WHERE source = 'builtin'
  AND model_id = 'claude-haiku-4-5'
  AND EXISTS (
      SELECT 1 FROM models WHERE model_id = 'claude-haiku-4-5-20251001'
  );
