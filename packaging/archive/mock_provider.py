#!/usr/bin/env python3
"""README 的本机固定响应 fixture；不连接任何外部服务。"""
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Fixture(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path != "/v1/models":
            self.send_error(404)
            return
        self.reply("application/json", json.dumps({"object": "list", "data": [{"id": "mock-local-model", "object": "model"}]}).encode())

    def do_POST(self):
        if self.path != "/v1/chat/completions":
            self.send_error(404)
            return
        request = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        common = {"id": "mock-local", "model": "mock-local-model"}
        if request.get("stream"):
            chunks = [
                {**common, "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": {"role": "assistant", "content": "RIVO_MOCK_OK"}, "finish_reason": None}]},
                {**common, "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
            ]
            body = "".join("data: " + json.dumps(chunk) + "\n\n" for chunk in chunks) + "data: [DONE]\n\n"
            self.reply("text/event-stream", body.encode())
        else:
            self.reply("application/json", json.dumps({**common, "object": "chat.completion", "choices": [{"index": 0, "message": {"role": "assistant", "content": "RIVO_MOCK_OK"}, "finish_reason": "stop"}]}).encode())

    def reply(self, content_type, body):
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


if __name__ == "__main__":
    print("Rivo Mock: http://127.0.0.1:18780/v1 (Ctrl+C to stop)", flush=True)
    try:
        ThreadingHTTPServer(("127.0.0.1", 18780), Fixture).serve_forever()
    except KeyboardInterrupt:
        pass
