# reeve-storage

Two tiers. A hot in-memory ring buffer sized for what the cockpit
shows live, and a warm SQLite store (WAL mode, one file) for
completed traces, history, replay, cost aggregation, and the budget
resync. Migrations and retention live here.

Part of [Reeve](https://github.com/Dancode-188/reeve), the terminal
cockpit for AI agents.
