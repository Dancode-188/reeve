-- A trace interrupted by connection loss (grace period expiry or Reeve
-- shutdown) can be resumed if the agent returns quickly; a trace
-- interrupted by plain agent silence cannot, because nothing is coming.
ALTER TABLE traces ADD COLUMN resumable INTEGER NOT NULL DEFAULT 0;
