-- Bump built-in defaults without overwriting user-customized values.

UPDATE policies
SET rpm_limit = 50,
    weekly_budget_nanousd = 200000000000,
    monthly_budget_nanousd = 2000000000000,
    updated_at = CURRENT_TIMESTAMP
WHERE id = 1
  AND name = 'default'
  AND max_concurrent = 5
  AND rpm_limit = 30
  AND weekly_budget_nanousd = 50000000000
  AND monthly_budget_nanousd = 150000000000;

UPDATE settings
SET value = '2.1.100',
    updated_at = CURRENT_TIMESTAMP
WHERE key = 'cc_cli_version'
  AND value = '2.1.80';
