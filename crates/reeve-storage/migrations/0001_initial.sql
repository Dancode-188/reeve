-- Initial schema for Reeve's SQLite warm tier.
-- Applied automatically on startup by reeve-storage's migration runner.
-- Columns mirror the reeve-model entities exactly. See docs/adr/ for the
-- design decisions behind the less obvious shapes (7-state trace machine,
-- polymorphic cost_ledger/evaluation_results, replay reconstruction).

CREATE TABLE IF NOT EXISTS agents (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    framework       TEXT NOT NULL, -- langchain / openai_sdk / custom / etc.
    integration     TEXT NOT NULL, -- sdk / proxy / log
    status          TEXT NOT NULL DEFAULT 'idle',
    first_seen_at   INTEGER NOT NULL,
    last_seen_at    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS traces (
    id                  TEXT PRIMARY KEY,
    agent_id            TEXT NOT NULL REFERENCES agents(id),
    status              TEXT NOT NULL DEFAULT 'running',
    started_at          INTEGER NOT NULL,
    completed_at        INTEGER,
    root_span_id        TEXT,
    final_health_score  REAL -- written on completion
);

CREATE TABLE IF NOT EXISTS spans (
    id              TEXT PRIMARY KEY,
    trace_id        TEXT NOT NULL REFERENCES traces(id),
    parent_id       TEXT,
    operation       TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'in_flight',
    -- OTel-side timestamps (agent clock, used for duration calculation)
    start_time      INTEGER NOT NULL,
    end_time        INTEGER,
    -- Reeve-side timestamp (wall clock at arrival, used for replay ordering)
    arrived_at      INTEGER NOT NULL,
    attributes      TEXT, -- JSON blob, normalized OTel GenAI attributes
    raw_attributes  TEXT  -- JSON blob, catch-all for forward compatibility
);

CREATE TABLE IF NOT EXISTS span_events (
    id              TEXT PRIMARY KEY,
    span_id         TEXT NOT NULL REFERENCES spans(id),
    event_type      TEXT NOT NULL,
    occurred_at     INTEGER NOT NULL,
    content         TEXT -- NULL when privacy tier 1; message text when tier 2
);

CREATE TABLE IF NOT EXISTS cost_ledger (
    id              TEXT PRIMARY KEY,
    entity_id       TEXT NOT NULL,
    entity_type     TEXT NOT NULL, -- 'trace' or 'agent'
    input_tokens    INTEGER NOT NULL DEFAULT 0,
    output_tokens   INTEGER NOT NULL DEFAULT 0,
    total_cost_usd  REAL    NOT NULL DEFAULT 0.0,
    updated_at      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS evaluation_results (
    id                  TEXT PRIMARY KEY,
    target_id           TEXT NOT NULL,
    target_type         TEXT NOT NULL, -- 'span' or 'trace'
    metric              TEXT NOT NULL, -- 'loop_detection', 'faithfulness', etc.
    score               REAL NOT NULL,
    evaluator           TEXT NOT NULL, -- 'heuristic' / 'llm_judge' / 'statistical'
    judge_model_version TEXT,
    evaluated_at        INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS intervention_commands (
    id                   TEXT PRIMARY KEY,
    trace_id             TEXT NOT NULL REFERENCES traces(id),
    span_id              TEXT,
    policy_id            TEXT, -- NULL when human-issued
    command_type         TEXT NOT NULL, -- serialized CommandType (carries instruction/context inline)
    status               TEXT NOT NULL DEFAULT 'pending_confirmation',
    requires_confirmation INTEGER NOT NULL DEFAULT 1,
    issued_by            TEXT NOT NULL, -- 'human' or 'policy:rule_id'
    valid_until_ms       INTEGER NOT NULL,
    issued_at            INTEGER NOT NULL,
    acknowledged_at      INTEGER
);

CREATE TABLE IF NOT EXISTS intervention_outcomes (
    id                      TEXT PRIMARY KEY,
    command_id              TEXT NOT NULL REFERENCES intervention_commands(id),
    trace_id                TEXT NOT NULL REFERENCES traces(id),
    pre_intervention_score  REAL,
    post_intervention_score REAL,
    delta                   REAL, -- positive = improvement
    spans_measured          INTEGER,
    measured_at             INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS policy_rules (
    id                      TEXT PRIMARY KEY,
    name                    TEXT NOT NULL,
    scope                   TEXT NOT NULL DEFAULT 'global',
    trigger_condition       TEXT NOT NULL, -- evalexpr expression
    command_type            TEXT NOT NULL, -- serialized CommandType
    requires_confirmation   INTEGER NOT NULL DEFAULT 1,
    cooldown_secs           INTEGER NOT NULL DEFAULT 0,
    auto_confirm_after_secs INTEGER,
    enabled                 INTEGER NOT NULL DEFAULT 1,
    created_at              INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS span_notes (
    id              TEXT PRIMARY KEY,
    span_id         TEXT NOT NULL REFERENCES spans(id),
    content         TEXT NOT NULL,
    created_at      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS replay_records (
    id                TEXT PRIMARY KEY,
    trace_id          TEXT NOT NULL REFERENCES traces(id),
    content_captured  INTEGER NOT NULL DEFAULT 0,
    captured_at       INTEGER NOT NULL
);

-- Indexes for the hot query paths
CREATE INDEX IF NOT EXISTS idx_spans_trace_id       ON spans(trace_id);
CREATE INDEX IF NOT EXISTS idx_spans_arrived_at     ON spans(arrived_at);
CREATE INDEX IF NOT EXISTS idx_traces_agent_id      ON traces(agent_id);
CREATE INDEX IF NOT EXISTS idx_traces_status        ON traces(status);
CREATE INDEX IF NOT EXISTS idx_cost_entity          ON cost_ledger(entity_id, entity_type);
CREATE INDEX IF NOT EXISTS idx_eval_target          ON evaluation_results(target_id, target_type);
CREATE INDEX IF NOT EXISTS idx_eval_evaluated_at    ON evaluation_results(evaluated_at);
CREATE INDEX IF NOT EXISTS idx_commands_trace_id    ON intervention_commands(trace_id);
CREATE INDEX IF NOT EXISTS idx_commands_status      ON intervention_commands(status);
CREATE INDEX IF NOT EXISTS idx_outcomes_command_id  ON intervention_outcomes(command_id);
