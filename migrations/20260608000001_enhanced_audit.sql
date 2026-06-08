-- Per-key opt-in: when enabled, requests authenticated with this key
-- write an additional row into request_log_audits capturing the
-- resolved client network metadata. Default OFF for every existing
-- key; admin must explicitly toggle.
ALTER TABLE api_keys ADD COLUMN enhanced_audit_enabled INTEGER NOT NULL DEFAULT 0;

-- Sidecar table holding the audit snapshot for a single request log
-- row. Existence of a row here is the authoritative "this request was
-- audited" signal — request_logs itself stays clean.
--
-- ON DELETE CASCADE pairs with the existing PRAGMA foreign_keys=ON
-- (see src/db/mod.rs) so log retention sweeps automatically clean up
-- the sidecar.
CREATE TABLE request_log_audits (
    request_log_id      INTEGER PRIMARY KEY
        REFERENCES request_logs(id) ON DELETE CASCADE,
    peer_ip             TEXT,            -- TCP peer address (always recorded)
    client_ip           TEXT,            -- resolved client IP (peer or trusted XFF/XRI)
    ip_source           TEXT,            -- "peer" | "xff" | "xri"
    forwarded_chain     TEXT,            -- truncated raw X-Forwarded-For (audit visibility)
    user_agent          TEXT,            -- truncated UA header
    api_surface         TEXT,            -- "anthropic" | "openai"
    anthropic_version   TEXT,            -- anthropic-version header (if present)
    anthropic_beta      TEXT,            -- anthropic-beta header (truncated)
    content_length      INTEGER          -- request body Content-Length, if reported
);
