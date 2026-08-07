#!/usr/bin/env python3
"""Local-only browser fixture for win-domain-flow validation.

The server binds only to 127.0.0.1 and never contacts the public Internet.
It provides deterministic page, fetch, redirect, image, and attachment download
traffic so the browser extension can be tested without polluting user data.
"""

from __future__ import annotations

import argparse
import json
import os
import signal
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit

FETCH_BYTES = 2 * 1024 * 1024
IMAGE_BYTES = 128 * 1024
DOWNLOAD_BYTES = 384 * 1024


class FixtureHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, _format: str, *_args: object) -> None:
        return

    def _send_bytes(
        self,
        status: int,
        content_type: str,
        body: bytes,
        extra_headers: dict[str, str] | None = None,
    ) -> None:
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        if extra_headers:
            for key, value in extra_headers.items():
                self.send_header(key, value)
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)
            self.wfile.flush()

    def do_GET(self) -> None:  # noqa: N802
        path = urlsplit(self.path).path
        if path in ("/", "/index.html"):
            html = b"""<!doctype html>
<meta charset="utf-8">
<title>win-domain-flow E2E fixture</title>
<h1>Local fixture</h1>
<img id="fixture-image" src="/image.bin" alt="fixture">
<a id="fixture-download" href="/download.bin" download="domainflow-e2e-download.bin">download</a>
<script>
window.fixtureReady = false;
Promise.all([
  fetch('/payload.bin?source=fetch').then(r => r.arrayBuffer()),
  fetch('/redirect?source=redirect').then(r => r.arrayBuffer())
]).then(values => {
  window.fixtureBytes = values.reduce((n, x) => n + x.byteLength, 0);
  window.fixtureReady = true;
}).catch(err => {
  window.fixtureError = String(err);
  window.fixtureReady = true;
});
</script>
"""
            self._send_bytes(200, "text/html; charset=utf-8", html)
            return

        if path == "/payload.bin":
            self._send_bytes(200, "application/octet-stream", b"P" * FETCH_BYTES)
            return

        if path == "/image.bin":
            # Validity as an image is not required for Network accounting. The
            # browser will still perform and classify the request as an image.
            self._send_bytes(200, "image/png", b"I" * IMAGE_BYTES)
            return

        if path == "/redirect":
            self.send_response(302)
            self.send_header("Location", "/payload.bin?source=redirect-target")
            self.send_header("Content-Length", "0")
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            return

        if path == "/download.bin":
            self._send_bytes(
                200,
                "application/octet-stream",
                b"D" * DOWNLOAD_BYTES,
                {
                    "Content-Disposition": 'attachment; filename="domainflow-e2e-download.bin"'
                },
            )
            return

        if path == "/health":
            self._send_bytes(200, "application/json", b'{"ok":true}')
            return

        self._send_bytes(404, "text/plain; charset=utf-8", b"not found")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=0)
    args = parser.parse_args()

    server = ThreadingHTTPServer(("127.0.0.1", args.port), FixtureHandler)
    host, port = server.server_address
    stop = threading.Event()

    def request_stop(_signum: int, _frame: object) -> None:
        stop.set()
        threading.Thread(target=server.shutdown, daemon=True).start()

    if hasattr(signal, "SIGTERM"):
        signal.signal(signal.SIGTERM, request_stop)
    if hasattr(signal, "SIGINT"):
        signal.signal(signal.SIGINT, request_stop)

    print(
        json.dumps(
            {
                "product": "win-domain-flow-e2e-fixture",
                "pid": os.getpid(),
                "host": host,
                "port": port,
                "url": f"http://127.0.0.1:{port}/index.html",
            },
            ensure_ascii=False,
        ),
        flush=True,
    )

    try:
        server.serve_forever(poll_interval=0.1)
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
