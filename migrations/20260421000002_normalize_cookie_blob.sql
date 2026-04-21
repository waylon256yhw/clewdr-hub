-- Strip 'sessionKey=' prefix from legacy cookie_blob values.
--
-- ClewdrCookie::from_str normalizes user input to its inner form
-- (sk-ant-sid02-...AA). The /admin/accounts endpoints, however, used to
-- persist the raw trimmed string, so pastes like 'sessionKey=sk-ant-...'
-- were stored verbatim. The stale-write guard in update_account_metadata
-- filters by `cookie_blob LIKE '<inner-prefix>%'`, which silently missed
-- these legacy rows: probe bootstrap results (email / account_type /
-- organization_uuid) were parsed correctly but never landed in the row,
-- so affected accounts lost their subscription badge and had their
-- assembled oauth_token treated as absent (gated on organization_uuid).
UPDATE accounts
SET cookie_blob = substr(cookie_blob, 12)
WHERE cookie_blob LIKE 'sessionKey=%';
