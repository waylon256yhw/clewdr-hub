-- Bump default CC CLI version 2.1.100 -> 2.1.170 to match DEFAULT_CLI_VERSION
-- in src/stealth.rs. The runtime reads settings.cc_cli_version from the DB and
-- only falls back to the constant when the row is missing, so the constant bump
-- alone never took effect. Conditional on the previous built-in default so any
-- admin-customized value is preserved (mirrors 20260421000001).

UPDATE settings
SET value = '2.1.170',
    updated_at = CURRENT_TIMESTAMP
WHERE key = 'cc_cli_version'
  AND value = '2.1.100';
