-- evaluation_results holds a row only when a metric produced a number,
-- so every way of failing to produce one is stored as the same thing,
-- which is nothing. A metric that burned the full timeout and a metric
-- that was never offered to the judge are indistinguishable, and
-- compute() reads both as the metric not applying to the trace, which
-- is the one thing neither of them means.
--
-- This records the dispatch rather than the outcome, so coverage can be
-- attempted against succeeded instead of present against absent. It
-- covers only the causes that reach a dispatch: a metric that was never
-- sampled, or had no input, or was skipped with the backend off, still
-- has no row anywhere.
CREATE TABLE IF NOT EXISTS judge_attempts (
    id                  TEXT PRIMARY KEY,
    trace_id            TEXT NOT NULL,
    metric              TEXT NOT NULL,
    outcome             TEXT NOT NULL, -- 'scored' / 'no_verdict' / 'half_pair' / 'no_claims'
    reason              TEXT,          -- NULL when the outcome is 'scored'
    attempted_at        INTEGER NOT NULL,
    judge_model_version TEXT
);

CREATE INDEX IF NOT EXISTS idx_attempt_trace   ON judge_attempts(trace_id);
CREATE INDEX IF NOT EXISTS idx_attempt_metric  ON judge_attempts(metric, outcome);
