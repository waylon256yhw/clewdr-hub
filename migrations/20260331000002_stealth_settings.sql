-- Phase 4.5: Stealth settings seed data
INSERT OR IGNORE INTO settings (key, value, updated_at) VALUES
    ('cc_cli_version', '2.1.80', CURRENT_TIMESTAMP),
    ('cc_sdk_version', '0.74.0', CURRENT_TIMESTAMP),
    ('cc_node_version', 'v24.3.0', CURRENT_TIMESTAMP),
    ('cc_stainless_os', 'Linux', CURRENT_TIMESTAMP),
    ('cc_stainless_arch', 'x64', CURRENT_TIMESTAMP),
    ('cc_beta_flags', 'claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,context-management-2025-06-27,prompt-caching-scope-2026-01-05,advanced-tool-use-2025-11-20,effort-2025-11-24', CURRENT_TIMESTAMP),
    ('cc_billing_salt', '59cf53e54c78', CURRENT_TIMESTAMP);
