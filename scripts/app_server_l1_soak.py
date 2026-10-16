"""Local L1 soak for the stdio app-server with an in-process fake SSE provider.

It never contacts a real Provider. The report contains only counters, latency values,
error codes and a local evidence path; it deliberately excludes prompts, headers,
credentials, workspace paths and raw Provider frames.
"""
from __future__ import annotations

import argparse
import json
import os
import queue
import statistics
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


class FakeProvider(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"

    def do_POST(self) -> None:  # noqa: N802
        _ = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        chunks = [
            b'data: {"id":"fixture","object":"chat.completion.chunk","model":"fixture-model","choices":[{"index":0,"delta":{"role":"assistant","content":"stream"},"finish_reason":null}]}\n\n',
            b'data: {"id":"fixture","object":"chat.completion.chunk","model":"fixture-model","choices":[{"index":0,"delta":{"content":" ok"},"finish_reason":null}]}\n\n',
            b'data: {"id":"fixture","object":"chat.completion.chunk","model":"fixture-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}\n\n',
            b"data: [DONE]\n\n",
        ]
        for chunk in chunks:
            self.wfile.write(chunk)
            self.wfile.flush()
            time.sleep(0.02)

    def log_message(self, *_args) -> None:
        return


def line(proc: subprocess.Popen, message: dict) -> None:
    assert proc.stdin
    proc.stdin.write(json.dumps(message, separators=(",", ":")) + "\n")
    proc.stdin.flush()


def until(proc: subprocess.Popen, request_id: int, terminal: bool = False) -> tuple[dict, list[dict]]:
    assert proc.stdout
    events: list[dict] = []
    response: dict | None = None
    while response is None or (terminal and not any(e.get("params", {}).get("type") == "turn_complete" for e in events)):
        raw = proc.stdout.readline()
        if not raw:
            raise RuntimeError("app-server exited before required protocol message")
        message = json.loads(raw)
        if message.get("id") == request_id:
            response = message
        elif message.get("method") == "event":
            events.append(message)
    return response, events


def worker(index: int, binary: Path, config: Path, data_dir: Path, workspace: Path, stop: float, result: queue.Queue) -> None:
    env = os.environ.copy()
    env.update({"RIVO_CONFIG": str(config), "RIVO_DATA_DIR": str(data_dir / f"instance-{index}"), "RIVO_SOAK_KEY": "local-fixture-key"})
    proc = subprocess.Popen([str(binary), "app-server", "--stdio"], text=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
    latencies: list[float] = []
    errors: dict[str, int] = {}
    completed = 0
    try:
        line(proc, {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"rivo-app-server-v1","client":{"name":"l1-soak","version":"1"}}})
        until(proc, 1)
        line(proc, {"jsonrpc":"2.0","id":2,"method":"session/start","params":{"working_directory":str(workspace),"runtime_settings":{"collaboration_mode":"code","permission_preset":"workspace_write"},"idempotency_key":f"start-{index}"}})
        started, _ = until(proc, 2)
        session_ref = started["result"]["session_ref"]
        turn = 0
        while time.monotonic() < stop:
            turn += 1
            begin = time.monotonic()
            line(proc, {"jsonrpc":"2.0","id":1000 + turn,"method":"turn/start","params":{"session_ref":session_ref,"prompt":"fixture","runtime_settings":{},"idempotency_key":f"turn-{index}-{turn}"}})
            response, events = until(proc, 1000 + turn, terminal=True)
            if "error" in response:
                code = response["error"].get("code", "unknown")
                errors[code] = errors.get(code, 0) + 1
                continue
            outcomes = [e.get("params", {}).get("payload", {}).get("outcome") for e in events if e.get("params", {}).get("type") == "turn_complete"]
            if outcomes != ["completed"]:
                code = "unexpected_terminal"
                errors[code] = errors.get(code, 0) + 1
                continue
            completed += 1
            latencies.append((time.monotonic() - begin) * 1000)
    except Exception as exc:  # summarized only; no raw provider/process output
        errors[type(exc).__name__] = errors.get(type(exc).__name__, 0) + 1
    finally:
        if proc.stdin:
            proc.stdin.close()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
    result.put((completed, latencies, errors, proc.returncode))


def percentile(values: list[float], percentage: float) -> float:
    if not values:
        return 0.0
    values = sorted(values)
    return values[min(len(values) - 1, round((len(values) - 1) * percentage))]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, default=Path(__file__).parents[1] / "runtime" / "target" / "debug" / "rivo.exe")
    parser.add_argument("--instances", type=int, default=5, choices=range(5, 11))
    parser.add_argument("--duration-seconds", type=int, default=300)
    parser.add_argument("--report", type=Path, default=Path(__file__).parents[1] / "artifacts" / "test-reports" / "app-server-l1-soak.json")
    args = parser.parse_args()
    if not args.binary.is_file():
        raise SystemExit(f"binary not found: {args.binary}")
    http = ThreadingHTTPServer(("127.0.0.1", 0), FakeProvider)
    threading.Thread(target=http.serve_forever, daemon=True).start()
    with tempfile.TemporaryDirectory(prefix="rivo-app-server-soak-") as root:
        root = Path(root)
        workspace = root / "workspace"
        workspace.mkdir()
        config = root / "config.toml"
        config.write_text(f'''version = 2
[defaults]
profile = "fixture"
[endpoints.fixture]
base_url = "http://127.0.0.1:{http.server_port}/v1"
[profiles.fixture]
protocol = "chat_completions"
endpoint = "fixture"
model = "fixture-model"
[profiles.fixture.auth]
env = "RIVO_SOAK_KEY"
''', encoding="utf-8")
        data = root / "data"
        data.mkdir()
        results: queue.Queue = queue.Queue()
        deadline = time.monotonic() + args.duration_seconds
        threads = [threading.Thread(target=worker, args=(i, args.binary, config, data, workspace, deadline, results)) for i in range(args.instances)]
        for thread in threads: thread.start()
        for thread in threads: thread.join()
    http.shutdown()
    aggregate = [results.get() for _ in threads]
    latencies = [latency for _, values, _, _ in aggregate for latency in values]
    errors: dict[str, int] = {}
    for _, _, codes, _ in aggregate:
        for code, count in codes.items(): errors[code] = errors.get(code, 0) + count
    completed = sum(value[0] for value in aggregate)
    attempted = completed + sum(errors.values())
    report = {"kind":"app-server-l1-fake-provider","instances":args.instances,"duration_seconds":args.duration_seconds,"attempted_turns":attempted,"completed_turns":completed,"success_rate":completed / attempted if attempted else 0.0,"latency_ms":{"p50":percentile(latencies, .50),"p95":percentile(latencies, .95),"p99":percentile(latencies, .99)},"errors":errors,"process_exit_codes":[value[3] for value in aggregate],"sensitive_data":"not recorded"}
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(json.dumps({"report": str(args.report), "success_rate": report["success_rate"]}))
    return 0 if report["success_rate"] >= .99 and not errors else 1


if __name__ == "__main__":
    raise SystemExit(main())
