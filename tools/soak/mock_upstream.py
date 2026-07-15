"""A fast streaming Anthropic mock for the soak: answers every Messages
request with a short SSE response carrying real usage, so the pipeline
prices, threads, and scores exactly as it would on live traffic. No
sleeps: the soak wants volume, not pacing."""
import json
import random
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODELS = ["claude-fable-5", "claude-opus-4-1", "gpt-5.4-mini"]


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", 0)))
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("transfer-encoding", "chunked")
        self.end_headers()
        model = random.choice(MODELS)

        def event(name, data):
            payload = f"event: {name}\ndata: {json.dumps(data)}\n\n".encode()
            self.wfile.write(f"{len(payload):x}\r\n".encode())
            self.wfile.write(payload + b"\r\n")

        event("message_start", {"type": "message_start", "message": {
            "model": model,
            "usage": {"input_tokens": random.randint(500, 4000),
                      "cache_read_input_tokens": random.randint(0, 3000),
                      "cache_creation_input_tokens": 0}}})
        for chunk in ("Working ", "through ", "the request."):
            event("content_block_delta", {"type": "content_block_delta",
                  "delta": {"type": "text_delta", "text": chunk}})
        want_tool = random.random() < 0.5
        if want_tool:
            event("content_block_start", {"type": "content_block_start", "index": 1,
                  "content_block": {"type": "tool_use", "id": "toolu_soak",
                                    "name": random.choice(["Read", "Bash", "web_search"]),
                                    "input": {"q": random.randint(0, 9999)}}})
        event("message_delta", {"type": "message_delta",
              "delta": {"stop_reason": "tool_use" if want_tool else "end_turn"},
              "usage": {"output_tokens": random.randint(20, 400)}})
        event("message_stop", {"type": "message_stop"})
        self.wfile.write(b"0\r\n\r\n")

    def log_message(self, *a):
        pass


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", 9996), H).serve_forever()
