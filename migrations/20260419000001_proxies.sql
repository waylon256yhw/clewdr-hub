CREATE TABLE proxies (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    protocol TEXT NOT NULL CHECK (protocol IN ('http', 'https', 'socks5', 'socks5h')),
    host TEXT NOT NULL,
    port INTEGER NOT NULL CHECK (port > 0 AND port <= 65535),
    username TEXT,
    password TEXT,
    last_test_success INTEGER,
    last_test_latency_ms INTEGER,
    last_test_message TEXT,
    last_test_ip_address TEXT,
    last_test_country TEXT,
    last_test_region TEXT,
    last_test_city TEXT,
    last_test_at TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

ALTER TABLE accounts ADD COLUMN proxy_id INTEGER REFERENCES proxies(id) ON DELETE SET NULL;

CREATE INDEX idx_accounts_proxy_id ON accounts(proxy_id);
