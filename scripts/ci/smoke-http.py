#!/usr/bin/env python3
"""Check HTTP features in the installed production package in CI."""

import gzip
import json
import os
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request


def fetch(path, timeout=3):
    with urllib.request.urlopen("http://127.0.0.1:9000" + path, timeout=timeout) as response:
        assert response.status == 200, (path, response.status)
        body = response.read(1024 * 1024)
        if response.headers.get("Content-Encoding") == "gzip":
            body = gzip.decompress(body)
        return response.headers, body


def main():
    binary = sys.argv[1]
    process = subprocess.Popen(
        [binary, "--listen-on", "127.0.0.1:0", "--workers", "1"],
        env={**os.environ, "RUST_LOG": "warn"},
    )
    try:
        deadline = time.monotonic() + 30
        while True:
            if process.poll() is not None:
                raise RuntimeError("server exited before HTTP became available")
            try:
                headers, body = fetch("/status", timeout=1)
                break
            except (urllib.error.URLError, TimeoutError):
                if time.monotonic() >= deadline:
                    raise
                time.sleep(0.1)

        assert headers.get_content_type() == "application/json"
        status = json.loads(body)
        assert status["nodes"] >= 0 and status["sessions"] >= 0, status

        headers, body = fetch("/ui/")
        assert headers.get_content_type() == "text/html"
        assert b"<html" in body.lower(), "UI HTML is missing"

        headers, body = fetch("/metrics")
        assert headers.get_content_type() == "text/plain"
        assert b"# TYPE " in body, "Prometheus metrics are missing"

        with urllib.request.urlopen("http://127.0.0.1:9000/events", timeout=3) as response:
            assert response.status == 200
            assert response.headers.get_content_type() == "text/event-stream"
            assert response.headers.get("Cache-Control") == "no-cache"

        assert process.poll() is None, "server exited during HTTP checks"
        print("Production HTTP checks passed: /ui/, /status, /metrics, /events (SSE)")
    finally:
        if process.poll() is None:
            process.send_signal(signal.SIGINT)
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


if __name__ == "__main__":
    main()
