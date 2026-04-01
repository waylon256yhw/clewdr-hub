-- Add telemetry metadata columns to accounts table
ALTER TABLE accounts ADD COLUMN email TEXT;
ALTER TABLE accounts ADD COLUMN account_type TEXT;
