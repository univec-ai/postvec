#!/usr/bin/env python3
"""A deterministic OpenAI-shaped embeddings mock for the PV-13 packaged
provider gate (tests/provider-e2e-test.sh).

Test-only: copied into the disposable test container at run time, never into
a package or image. Python 3 standard library only, because the target
containers can install `python3` from their own archives and nothing else.

Wire shape mirrors what the OpenAI connector actually reads (see
providers/src/testing.rs and providers/src/openai.rs): `POST /v1/embeddings`
answers `{"data": [{"embedding": [...], "index": i}, ...]}`. Vectors are a
deterministic hashed bag-of-words of the input text, normalised — so the same
text always embeds identically, and a query sharing words with a document has
genuinely higher cosine similarity, which is what makes the search-ranking
assertion an assertion.

It is also a minimal UniVec (aphex) for `provider add univec` discovery:
    GET  /v1/models              -> the public catalogue (one embed model,
                                    one converter from the OpenAI mock model
                                    into CONVERT_TARGET, argv[2])
    POST /v1/convert             -> {"success": true, "data": {"embeddings"}}
                                    at the converter's target dimension
    GET  /v1/registry/index.json -> the free identity check: 200 when the
                                    bearer matches, 401 otherwise

Control interface (never reached by the code under test):
    GET  /healthz          -> 200 once serving
    GET  /control          -> JSON state {requests, started, in_flight,
                              mode, auth_failures}
    POST /control          -> JSON body, any of:
        {"mode": "success" | "oversized" | "trickle" | "hold"}
        {"statuses": [429, 500]}   # per-request status queue, then success
        {"require_bearer": "key"}  # 401 anything else; "" disables
        {"catalog": "ok" | "empty" | "500"}   # what GET /v1/models answers
        {"reset": true}            # zero the counters
        {"release": true}          # release every held request

The credential is compared, counted on mismatch, and never printed.
"""

import hashlib
import json
import struct
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

CONVERT_TARGET = sys.argv[2] if len(sys.argv) > 2 else "sentence-transformers-all-minilm-l6-v2"
SOURCE_MODEL = "openai-text-embedding-3-small"   # the mock's own public name in postvec


def catalogue():
    """The aphex `PublicModelView` shape, dims matching this mock's vectors."""
    return [
        {"name": "mock-embed", "modelType": "embed", "executionProvider": "cpu",
         "targetModel": "mock-embed", "targetDim": 1536, "sequenceLen": 512},
        {"name": f"convert-{SOURCE_MODEL}-to-{CONVERT_TARGET}", "modelType": "convert",
         "executionProvider": "cpu", "sourceModel": SOURCE_MODEL, "sourceDim": 1536,
         "targetModel": CONVERT_TARGET, "targetDim": 384,
         "eval": {"cosine_mean": 0.91}},
        {"name": "embed-bridge", "modelType": "embed-bridge", "executionProvider": "cpu"},
    ]


STATE = {
    "requests": 0,        # total /v1/embeddings requests received
    "converts": 0,        # total /v1/convert requests received
    "catalog_requests": 0,
    "catalog": "ok",
    "started": 0,         # requests that began processing (incl. held)
    "in_flight": 0,       # currently blocked "hold" requests
    "auth_failures": 0,
    "mode": "success",
    "statuses": [],       # queue of statuses to answer with, then success
    "require_bearer": "",
}
LOCK = threading.Lock()
RELEASE = threading.Event()


def vector_for(text: str, dim: int):
    """Hashed bag-of-words, L2-normalised; zero-safe for wordless input."""
    v = [0.0] * dim
    for token in "".join(c.lower() if c.isalnum() else " " for c in text).split():
        digest = hashlib.sha256(token.encode()).digest()
        (idx,) = struct.unpack_from(">I", digest)
        v[idx % dim] += 1.0
    norm = sum(c * c for c in v) ** 0.5
    if norm == 0.0:
        v[0] = 1.0
        norm = 1.0
    return [round(c / norm, 6) for c in v]


def fold(vector, dim):
    """Deterministic conversion: fold the input onto `dim`, L2-normalise."""
    out = [0.0] * dim
    for i, c in enumerate(vector):
        out[i % dim] += float(c)
    norm = sum(c * c for c in out) ** 0.5 or 1.0
    return [round(c / norm, 6) for c in out]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):  # quiet; the harness reads /control
        pass

    def _json(self, status, payload):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)

    def _read_body(self):
        length = int(self.headers.get("Content-Length", "0") or "0")
        return self.rfile.read(length) if length else b""

    # ---- control -----------------------------------------------------------

    def _bearer_ok(self):
        with LOCK:
            required = STATE["require_bearer"]
        if required and self.headers.get("Authorization") != f"Bearer {required}":
            with LOCK:
                STATE["auth_failures"] += 1
            return False
        return True

    def do_GET(self):
        if self.path == "/healthz":
            self._json(200, {"ok": True})
            return
        if self.path == "/v1/models":
            with LOCK:
                STATE["catalog_requests"] += 1
                mode = STATE["catalog"]
            if mode == "500":
                self._json(500, {"success": False, "error": "catalogue unavailable"})
            else:
                self._json(200, {"success": True, "data": [] if mode == "empty" else catalogue()})
            return
        if self.path == "/v1/registry/index.json":
            if self._bearer_ok():
                self._json(200, {"channel": "authenticated", "models": []})
            else:
                self._json(401, {"error": {"message": "invalid api key"}})
            return
        if self.path == "/control":
            with LOCK:
                snapshot = {k: v for k, v in STATE.items() if k != "require_bearer"}
                snapshot["require_bearer_set"] = bool(STATE["require_bearer"])
            self._json(200, snapshot)
            return
        self._json(404, {"error": "no such path"})

    # ---- the API under test ------------------------------------------------

    def do_POST(self):
        body = self._read_body()
        if self.path == "/control":
            directive = json.loads(body or b"{}")
            with LOCK:
                if directive.get("reset"):
                    STATE.update(requests=0, converts=0, catalog_requests=0, started=0,
                                 auth_failures=0, statuses=[])
                if "catalog" in directive:
                    STATE["catalog"] = directive["catalog"]
                if "mode" in directive:
                    STATE["mode"] = directive["mode"]
                    if directive["mode"] == "hold":
                        RELEASE.clear()
                if "statuses" in directive:
                    STATE["statuses"] = list(directive["statuses"])
                if "require_bearer" in directive:
                    STATE["require_bearer"] = directive["require_bearer"]
            if directive.get("release"):
                RELEASE.set()
            self._json(200, {"ok": True})
            return

        if self.path == "/v1/convert":
            with LOCK:
                STATE["converts"] += 1
            if not self._bearer_ok():
                self._json(401, {"error": {"message": "invalid api key"}})
                return
            request = json.loads(body or b"{}")
            route = next((m for m in catalogue() if m["modelType"] == "convert"
                          and m["sourceModel"] == request.get("source_model")
                          and m["targetModel"] == request.get("target_model")), None)
            if route is None:
                self._json(404, {"success": False, "error": "no such converter"})
                return
            out = [fold(v, route["targetDim"]) for v in request.get("embeddings", [])]
            self._json(200, {"success": True, "data": {"embeddings": out}})
            return

        if self.path != "/v1/embeddings":
            self._json(404, {"error": {"message": "no such path"}})
            return

        with LOCK:
            STATE["requests"] += 1
            required = STATE["require_bearer"]
            mode = STATE["mode"]
            queued = STATE["statuses"].pop(0) if STATE["statuses"] else None

        if required and self.headers.get("Authorization") != f"Bearer {required}":
            with LOCK:
                STATE["auth_failures"] += 1
            self._json(401, {"error": {"message": "invalid api key"}})
            return

        with LOCK:
            STATE["started"] += 1

        if queued is not None:
            self._json(int(queued), {"error": {"message": f"mock status {queued}"}})
            return

        if mode == "oversized":
            # 200 with no Content-Length, streamed until the client hangs up:
            # the shape a Content-Length check cannot defend against. Capped
            # server-side so a broken client cannot melt the test host.
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Transfer-Encoding", "chunked")
            self.send_header("Connection", "close")
            self.end_headers()
            chunk = b"x" * 65536
            written = 0
            try:
                while written < 512 * 1024 * 1024:
                    self.wfile.write(b"%x\r\n%s\r\n" % (len(chunk), chunk))
                    written += len(chunk)
            except OSError:
                pass
            return

        if mode == "trickle":
            # A response that never finishes; the caller's deadline is the
            # only way out. Bounded at 10 minutes server-side.
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Transfer-Encoding", "chunked")
            self.send_header("Connection", "close")
            self.end_headers()
            try:
                for _ in range(600):
                    self.wfile.write(b"1\r\nx\r\n")
                    self.wfile.flush()
                    threading.Event().wait(1.0)
            except OSError:
                pass
            return

        if mode == "hold":
            with LOCK:
                STATE["in_flight"] += 1
            try:
                RELEASE.wait(timeout=600)
            finally:
                with LOCK:
                    STATE["in_flight"] -= 1
            # fall through to a success response (the client may be long gone)

        request = json.loads(body or b"{}")
        inputs = request.get("input", [])
        if isinstance(inputs, str):
            inputs = [inputs]
        dim = int(request.get("dimensions") or 1536)
        data = [
            {"embedding": vector_for(text, dim), "index": i}
            for i, text in enumerate(inputs)
        ]
        self._json(
            200,
            {
                "object": "list",
                "data": data,
                "model": request.get("model", ""),
                "usage": {"prompt_tokens": 0, "total_tokens": 0},
            },
        )


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8099
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    server.daemon_threads = True
    print(f"provider-mock: serving on 127.0.0.1:{port}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
