-- Initial schema for Reeve's SQLite warm tier.
-- Applied automatically on startup via sqlx migrations.

CREATE TABLE IF NOT EXISTS agents (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    integration     TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'idle',
    first_seen_at   INTEGER NOT NULL,
    last_seen_at    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS traces (
    id              TEXT PRIMARY KEY,
    agent_id        TEXT NOT NULL REFERENCES agents(id),
    status          TEXT NOT NULL DEFAULT 'in_flight',
    started_at      INTEGER NOT NULL,
    completed_at    INTEGER,
    root_span_id    TEXT
);

CREATE TABLE IF NOT EXISTS spans (
    id              TEXT PRIMARY KEY,
    trace_id        TEXT NOT NULL REFERENCES traces(id),
    parent_id       TEXT,
    name            TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'in_flight',
    -- OTel-side timestamps (agent clock, used for duration calculation)
    start_time      INTEGER NOT NULL,
    end_time        INTEGER,
    -- Reeve-side timestamp (wall clock at arrival, used for replay ordering)
    arrived_at      INTEGER NOT NULL,
    attributes      TEXT     -- JSON blob
);

CREATE TABLE IF NOT EXISTS span_events (
    id              TEXT PRIMARY KEY,
    span_id         TEXT NOT NULL REFERENCES spans(id),
    event_type      TEXT NOT NULL,
    timestamp       INTEGER NOT NULL,
    payload         TEXT     -- JSON blob
);

CREATE TABLE IF NOT EXISTS cost_ledger (
    id              TEXT PRIMARY KEY,
    -- A ledger entry is scoped to a trace, but we also roll up per agent.
    trace_id        TEXT NOT NULL REFERENCES traces(id),
    agent_id        TEXT NOT NULL REFERENCES agents(id),
    -- Token counts and cost in USD
    input_tokens    INTEGER NOT NULL DEFAULT 0,
    output_tokens   INTEGER NOT NULL DEFAULT 0,
    total_cost_usd  REAL    NOT NULL DEFAULT 0.0,
    -- When this entry was last updated (accumulator flushes here)
    updated_at      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS evaluation_results (
    id              TEXT PRIMARY KEY,
    trace_id        TEXT NOT NULL REFERENCES traces(id),
    evaluator_type  TEXT NOT NULL,
    score           REAL,
    flags           TEXT,    -- JSON array of flag strings
    evaluated_at    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS intervention_commands (
    id              TEXT PRIMARY KEY,
    trace_id        TEXT NOT NULL REFERENCES traces(id),
    span_id         TEXT,
    command_type    TEXT NOT NULL,
    payload         TEXT,    -- JSON
    status          TEXT NOT NULL DEFAULT 'pending',
    issued_at       INTEGER NOT NULL,
    acked_at        INTEGER
);

CREATE TABLE IF NOT EXISTS intervention_outcomes (
    id              TEXT PRIMARY KEY,
    command_id      TEXT NOT NULL REFERENCES intervention_commands(id),
    trace_id        TEXT NOT NULL REFERENCES traces(id),
    -- Health score immediately before the intervention fired
    score_before    REAL,
    -- Health score after N spans have passed post-intervention
    score_after     REAL,
    -- How many spans elapsed before score_after was sampled
    spans_elapsed   INTEGER,
    -- Whether the outcome was considered an improvement
    improved        INTEGER, -- 0 or 1
    recorded_at     INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS policy_rules (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    scope           TEXT NOT NULL DEFAULT 'global',
    condition       TEXT NOT NULL, -- evalexpr expression
    action          TEXT NOT NULL,
    enabled         INTEGER NOT NULL DEFAULT 1,
    created_at      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS span_notes (
    id              TEXT PRIMARY KEY,
    span_id         TEXT NOT NULL REFERENCES spans(id),
    content         TEXT NOT NULL,
    created_at      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS replay_records (
    id              TEXT PRIMARY KEY,
    trace_id        TEXT NOT NULL REFERENCES traces(id),
    recorded_at     INTEGER NOT NULL,
    events          TEXT NOT NULL  -- JSON array of replay events
);

-- Indexes for the hot query paths
CREATE INDEX IF NOT EXISTS idx_spans_trace_id       ON spans(trace_id);
CREATE INDEX IF NOT EXISTS idx_spans_arrived_at     ON spans(arrived_at);
CREATE INDEX IF NOT EXISTS idx_traces_agent_id      ON traces(agent_id);
CREATE INDEX IF NOT EXISTS idx_traces_status        ON traces(status);
CREATE INDEX IF NOT EXISTS idx_cost_trace_id        ON cost_ledger(trace_id);
CREATE INDEX IF NOT EXISTS idx_cost_agent_id        ON cost_ledger(agent_id);
CREATE INDEX IF NOT EXISTS idx_eval_trace_id        ON evaluation_results(trace_id);
CREATE INDEX IF NOT EXISTS idx_commands_trace_id    ON intervention_commands(trace_id);
CREATE INDEX IF NOT EXISTS idx_commands_status      ON intervention_commands(status);
CREATE INDEX IF NOT EXISTS idx_outcomes_command_id  ON intervention_outcomes(command_id);
