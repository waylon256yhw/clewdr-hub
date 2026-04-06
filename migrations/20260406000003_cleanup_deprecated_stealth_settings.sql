-- Remove deprecated stealth settings that no longer affect the Claude API path.
DELETE FROM settings
WHERE key IN (
    'cc_sdk_version',
    'cc_node_version',
    'cc_stainless_os',
    'cc_stainless_arch',
    'cc_beta_flags'
);
