"""A fake OpenAI-compatible chat server for TUI tests. It answers each request with the next
scripted reply, streamed as server-sent events, and keeps every request it was sent.

A reply is a list of steps, each a chunk (a dict, sent as one event) or a number of seconds to
wait; `reply()` builds the usual ones. Standard library only.
"""

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def reply(reasoning="", text="", calls=(), wait=0.0, stall=0.0, reasoning_tokens=None):
    """One reply: its reasoning, then its text, then its tool calls, given as (name, arguments)
    pairs, then the usage and timings radiance reports. `wait` pauses before the text. With
    `stall`, the reply stops after its reasoning and the connection stays open that long, as a
    server does while it is still thinking."""
    steps = [delta(reasoning_content=word) for word in pieces(reasoning)]
    if stall:
        return steps + [STALL, stall]
    if wait:
        steps.append(wait)
    steps += [delta(content=word) for word in pieces(text)]
    for i, (name, arguments) in enumerate(calls):
        call = {"index": i, "id": f"call_{i}", "type": "function"}
        call["function"] = {"name": name, "arguments": json.dumps(arguments)}
        steps.append(delta(tool_calls=[call]))
    finish = "tool_calls" if calls else "stop"
    steps.append({"choices": [{"index": 0, "delta": {}, "finish_reason": finish}]})
    thinking = reasoning_tokens if reasoning_tokens is not None else len(reasoning.split())
    steps.append(
        {
            "choices": [],
            "usage": {
                "prompt_tokens": 1200,
                "completion_tokens": thinking + len(text.split()) + 10,
                "completion_tokens_details": {"reasoning_tokens": thinking},
            },
            "timings": {
                "cache_n": 1152,
                "prompt_n": 48,
                "prompt_ms": 12.0,
                "predicted_per_second": 150.0,
            },
        }
    )
    return steps


# Marks the point after which a stalled reply only waits.
STALL = "stall"


def delta(**fields):
    return {"choices": [{"index": 0, "delta": fields}]}


def pieces(text):
    """`text` in the small pieces a server streams, which join back to it exactly."""
    words = text.split(" ")
    return [w + " " for w in words[:-1]] + [words[-1]] if text else []


class Server:
    """Serves `replies` in order, one per request; the last one again if there are more requests.
    Subagents' requests, which offer the `finish` tool, are answered from `subagent` instead, so
    that a subagent can work alongside the agent that started it. `url` is the API root, and
    `requests` and `subagent_requests` hold each request's JSON body."""

    def __init__(self, replies, subagent=()):
        self.replies = replies
        self.subagent = list(subagent)
        self.requests = []
        self.subagent_requests = []
        self.lock = threading.Lock()
        self.closed = threading.Event()
        server = self

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers.get("content-length", 0))))
                tools = {t["function"]["name"] for t in body.get("tools", [])}
                with server.lock:
                    if server.subagent and "finish" in tools:
                        requests, replies = server.subagent_requests, server.subagent
                    else:
                        requests, replies = server.requests, server.replies
                    requests.append(body)
                    steps = replies[min(len(requests), len(replies)) - 1]
                self.send_response(200)
                self.send_header("content-type", "text/event-stream")
                self.end_headers()
                try:
                    stalled = False
                    for step in steps:
                        if step == STALL:
                            stalled = True
                        elif isinstance(step, (int, float)):
                            server.closed.wait(step)
                        else:
                            self.wfile.write(f"data: {json.dumps(step)}\n\n".encode())
                            self.wfile.flush()
                    if not stalled:
                        self.wfile.write(b"data: [DONE]\n\n")
                except (BrokenPipeError, ConnectionResetError):
                    pass  # the client stopped listening: an interrupt

            def log_message(self, *args):
                pass

        self.http = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.http.daemon_threads = True
        self.url = f"http://127.0.0.1:{self.http.server_port}/v1"
        threading.Thread(target=self.http.serve_forever, daemon=True).start()

    def close(self):
        self.closed.set()
        self.http.shutdown()
        self.http.server_close()

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()
