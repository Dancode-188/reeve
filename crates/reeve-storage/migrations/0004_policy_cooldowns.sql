CREATE TABLE IF NOT EXISTS policy_cooldowns (
    agent_id      TEXT    NOT NULL,
    rule_id       TEXT    NOT NULL,
    last_fired_at INTEGER NOT NULL,
    expires_at    INTEGER NOT NULL,
    PRIMARY KEY (agent_id, rule_id)
);
