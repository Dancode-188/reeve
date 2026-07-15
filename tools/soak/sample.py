"""Samples the running Reeve process every interval and appends RSS,
open fd count, and warm-database size to a CSV. Give it the pid and the
db path; it runs until killed."""
import csv
import os
import sys
import time

pid = int(sys.argv[1])
db_path = sys.argv[2]
out = sys.argv[3] if len(sys.argv) > 3 else "soak_metrics.csv"
interval = int(os.environ.get("SOAK_SAMPLE_SECS", "60"))


def rss_kb(p: int) -> int:
    with open(f"/proc/{p}/status") as f:
        for line in f:
            if line.startswith("VmRSS:"):
                return int(line.split()[1])
    return 0


def fd_count(p: int) -> int:
    try:
        return len(os.listdir(f"/proc/{p}/fd"))
    except OSError:
        return -1


def db_bytes(path: str) -> int:
    total = 0
    for suffix in ("", "-wal", "-shm"):
        try:
            total += os.path.getsize(path + suffix)
        except OSError:
            pass
    return total


start = time.time()
with open(out, "w", newline="") as f:
    w = csv.writer(f)
    w.writerow(["elapsed_s", "rss_kb", "fd_count", "db_bytes"])
    f.flush()
    while True:
        if not os.path.exists(f"/proc/{pid}"):
            break
        row = [int(time.time() - start), rss_kb(pid), fd_count(pid),
               db_bytes(db_path)]
        w.writerow(row)
        f.flush()
        print("sample", row, flush=True)
        time.sleep(interval)
