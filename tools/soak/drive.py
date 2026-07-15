"""Drives sustained proxy traffic for the soak: many rolling
conversations, each threading a few turns then retiring, so the
pipeline sees turn threading, tool spans, concurrent agents, and a
steady churn of completed traces. Runs until killed."""
import json
import os
import random
import threading
import time
import urllib.request

URL = "http://127.0.0.1:4318/v1/messages"
CONCURRENCY = int(os.environ.get("SOAK_CONCURRENCY", "8"))
stop = threading.Event()


def one_conversation(worker: int) -> None:
    convo = random.randint(0, 1_000_000)
    history = [{"role": "user", "content": f"task {convo}"}]
    turns = random.randint(1, 6)
    for _ in range(turns):
        if stop.is_set():
            return
        body = json.dumps({"model": "claude-fable-5", "max_tokens": 1024,
                           "stream": True, "messages": history}).encode()
        req = urllib.request.Request(URL, body, {
            "content-type": "application/json",
            "user-agent": f"soak-agent-{worker}/1.0",
            "x-api-key": "soak"})
        tool_id = None
        try:
            resp = urllib.request.urlopen(req, timeout=30).read().decode()
        except Exception:
            return
        for line in resp.splitlines():
            if line.startswith("data: "):
                try:
                    d = json.loads(line[6:])
                except ValueError:
                    continue
                b = d.get("content_block", {})
                if b.get("type") == "tool_use":
                    tool_id = b["id"]
        if tool_id is None:
            return
        history.append({"role": "assistant", "content": [
            {"type": "tool_use", "id": tool_id, "name": "Read",
             "input": {"n": convo}}]})
        history.append({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": tool_id, "content": "ok"}]})
        # Think time between turns: real agents run tools between round
        # trips, and the soak measures endurance, not peak throughput.
        time.sleep(random.uniform(0.3, 0.8))


def worker(worker_id: int) -> None:
    while not stop.is_set():
        one_conversation(worker_id)
        time.sleep(random.uniform(0.05, 0.4))


def main() -> None:
    threads = [threading.Thread(target=worker, args=(i,), daemon=True)
               for i in range(CONCURRENCY)]
    for t in threads:
        t.start()
    try:
        while True:
            time.sleep(1)
    except KeyboardInterrupt:
        stop.set()


if __name__ == "__main__":
    main()
