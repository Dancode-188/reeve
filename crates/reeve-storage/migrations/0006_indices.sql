-- The budget resync windows traces by completed_at every 30 seconds,
-- which had no index. The index alone made things worse: without
-- statistics the planner flipped to scanning all spans as the outer
-- loop (measured 2.0s against 0.3s unindexed on a 624k span store),
-- so ANALYZE ships with it, and the pair takes the same window query
-- to under a millisecond.

CREATE INDEX IF NOT EXISTS idx_traces_completed_at ON traces(completed_at);
ANALYZE;
