-- Speed up the default request log list and recent-window counts.
-- Keep this intentionally narrow: one extra time index is enough for the
-- current small-team workload without adding much write amplification.
CREATE INDEX IF NOT EXISTS idx_request_logs_started
ON request_logs(started_at DESC);
