-- Add utilization percentage columns to account_runtime_state
ALTER TABLE account_runtime_state ADD COLUMN session_utilization REAL;
ALTER TABLE account_runtime_state ADD COLUMN weekly_utilization REAL;
ALTER TABLE account_runtime_state ADD COLUMN weekly_sonnet_utilization REAL;
ALTER TABLE account_runtime_state ADD COLUMN weekly_opus_utilization REAL;
