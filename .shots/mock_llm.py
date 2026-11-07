"""A stand-in OpenAI-compatible chat server for screenshots: it 'translates'
by tagging each line, so the whole pipeline can be watched without a model."""
import json, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

WORDS = {
    "good morning": "早上好", "everyone": "各位", "welcome": "欢迎", "quarterly": "季度",
    "product review": "产品评审", "today": "今天", "decoder": "解码器", "latency": "延迟",
    "half": "一半", "new": "新的",
}

def fake(line: str) -> str:
    out = [zh for en, zh in WORDS.items() if en in line.lower()]
    return ("，".join(out) or "译文") + "。"

class H(BaseHTTPRequestHandler):
    def do_GET(self):
        body = json.dumps({"data": [{"id": "qwen3:8b"}]}).encode()
        self.send_response(200); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)

    def do_POST(self):
        req = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        user = json.loads(req["messages"][-1]["content"])
        time.sleep(0.4)
        content = "<think>fine</think>" + json.dumps({"t": [fake(l) for l in user["lines"]]}, ensure_ascii=False)
        body = json.dumps({"choices": [{"message": {"role": "assistant", "content": content}}]}).encode()
        self.send_response(200); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)

    def log_message(self, *a):
        pass

ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1]) if len(sys.argv) > 1 else 11434), H).serve_forever()
