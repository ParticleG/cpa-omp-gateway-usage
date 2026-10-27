#!/usr/bin/env python3
"""Exercise the release library in real CPA with local synthetic quota data."""

import json
import secrets
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

CPA_IMAGE = "eceasy/cli-proxy-api:v8.0.43"
PLUGIN = "omp-gateway-usage"
USAGE_PATH = f"/v0/management/plugins/{PLUGIN}/usage"
RESOURCE_PATH = f"/v0/resource/plugins/{PLUGIN}/status"
GATEWAY_TOKEN = "synthetic-gateway-token"
PRIVATE_SENTINEL = "private-account@example.test"


def docker(*args):
    return subprocess.run(
        ["docker", *args], check=True, stdout=subprocess.PIPE, text=True
    ).stdout.strip()


class Gateway(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path != "/v1/usage" or self.headers.get("Authorization") != f"Bearer {GATEWAY_TOKEN}":
            self.send_error(403)
            return
        now = int(time.time() * 1000)
        body = json.dumps({
            "generatedAt": now,
            "reports": [{
                "provider": "anthropic",
                "fetchedAt": now,
                "metadata": {"email": PRIVATE_SENTINEL, "accessToken": GATEWAY_TOKEN},
                "limits": [{
                    "label": "Claude 5 Hour",
                    "window": {"durationMs": 18000000, "resetsAt": now + 18000000},
                    "amount": {"unit": "percent", "usedFraction": 0.25},
                }],
            }],
        }).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_args):
        pass


def fetch(base, path, token=None):
    request = urllib.request.Request(base + path)
    if token:
        request.add_header("Authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(request, timeout=15) as response:
            return response.status, response.read(), response.headers
    except urllib.error.HTTPError as error:
        return error.code, error.read(), error.headers


def main():
    if len(sys.argv) != 3:
        raise SystemExit("Usage: python3 scripts/smoke.py <library.so> <version>")
    library = Path(sys.argv[1]).resolve(strict=True)
    version = sys.argv[2]
    admin = secrets.token_urlsafe(32)
    caller = secrets.token_urlsafe(24)
    container = None
    gateway = ThreadingHTTPServer(("127.0.0.1", 0), Gateway)
    thread = threading.Thread(target=gateway.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="cpa-omp-usage-smoke-") as temporary:
            root = Path(temporary)
            token = root / "gateway.token"
            token.write_text(GATEWAY_TOKEN)
            token.chmod(0o600)
            with socket.socket() as reservation:
                reservation.bind(("127.0.0.1", 0))
                port = reservation.getsockname()[1]
            config = {
                "host": "127.0.0.1",
                "port": port,
                "auth-dir": "/tmp/smoke-auth",
                "remote-management": {
                    "allow-remote": False,
                    "secret-key": admin,
                    "disable-control-panel": True,
                },
                "access": {"api-keys": [caller]},
                "plugins": {
                    "enabled": True,
                    "dir": "/plugins",
                    "configs": {PLUGIN: {
                        "enabled": True,
                        "gateway_url": f"http://127.0.0.1:{gateway.server_port}",
                        "token_file": "/run/secrets/gateway.token",
                    }},
                },
            }
            config_path = root / "config.yaml"
            config_path.write_text(json.dumps(config))
            config_path.chmod(0o600)
            container = docker(
                "run", "--detach", "--network", "host",
                "--volume", f"{config_path}:/CLIProxyAPI/config.yaml",
                "--volume", f"{token}:/run/secrets/gateway.token:ro",
                "--volume", f"{library}:/plugins/linux/amd64/{PLUGIN}.so:ro",
                CPA_IMAGE,
            )
            base = f"http://127.0.0.1:{port}"
            try:
                deadline = time.monotonic() + 30
                while True:
                    try:
                        status, body, _ = fetch(base, "/v8/management/plugins", admin)
                        assert status == 200, f"Management readiness returned {status}"
                        break
                    except urllib.error.URLError:
                        if time.monotonic() >= deadline:
                            raise RuntimeError("CPA did not become ready")
                        time.sleep(0.25)
                plugin = next(item for item in json.loads(body)["plugins"] if item["id"] == PLUGIN)
                assert plugin["registered"] and plugin["effective_enabled"], "Native plugin failed registration"
                assert plugin["metadata"]["version"] == version, "Embedded version differs from release version"
                status, body, _ = fetch(base, RESOURCE_PATH)
                assert status == 200 and b"OMP Gateway Usage" in body
                assert PRIVATE_SENTINEL.encode() not in body and GATEWAY_TOKEN.encode() not in body
                assert fetch(base, USAGE_PATH)[0] == 401, "Anonymous quota access was accepted"
                assert fetch(base, USAGE_PATH, caller)[0] == 401, "Inference key gained management access"
                status, body, headers = fetch(base, USAGE_PATH, admin)
                assert status == 200, f"Authenticated quota query returned {status}"
                assert headers.get("Cache-Control") == "no-store"
                assert PRIVATE_SENTINEL.encode() not in body and GATEWAY_TOKEN.encode() not in body
                limits = json.loads(body)["reports"][0]["limits"]
                assert limits[0]["usedPercent"] == 25 and limits[0]["remainingPercent"] == 75
                print(f"PASS: CPA {CPA_IMAGE}, native version {version}, quota projection, management boundary")
            except Exception:
                logs = docker("logs", container)
                for secret in (admin, caller, GATEWAY_TOKEN):
                    logs = logs.replace(secret, "[REDACTED]")
                print(logs[-6000:], file=sys.stderr)
                raise
            finally:
                docker("rm", "--force", container)
                container = None
    finally:
        if container:
            docker("rm", "--force", container)
        gateway.shutdown()
        gateway.server_close()
        thread.join()


if __name__ == "__main__":
    main()
