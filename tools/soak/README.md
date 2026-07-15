# Soak harness

A long-running load test for Reeve. Points sustained mock traffic at
the proxy and samples memory, file descriptors, and database size, so a
leak or unbounded growth shows up over hours rather than in production.

Run it (from the repo root, with a release build):

```bash
# terminal 1: the mock upstream
python3 tools/soak/mock_upstream.py

# terminal 2: Reeve, pointed at the mock
REEVE_PROXY_UPSTREAM=http://127.0.0.1:9996 reeve

# terminal 3: the load and the sampler
python3 tools/soak/drive.py &
python3 tools/soak/sample.py "$(pgrep -f target/release/reeve)" \
    ~/.local/share/reeve/reeve.db soak_metrics.csv
```

`SOAK_CONCURRENCY` sets the number of rolling conversations (default 8);
`SOAK_SAMPLE_SECS` sets the sample interval (default 60). Pass criteria
live in the issue that introduced this. Read `soak_metrics.csv` after:
RSS should flatten after warmup, fd count should hold steady, and the
database should grow with traces stored rather than faster.
