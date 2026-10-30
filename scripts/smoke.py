#!/usr/bin/env python3
"""Verify a packaged plugin ZIP against the official native CPA v8.0.43 build."""

import hashlib
import json
import os
import platform
import re
import secrets
import shutil
import socket
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import urllib.error
import urllib.request
import zipfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

CPA_VERSION = "8.0.43"
CPA_TAG = f"v{CPA_VERSION}"
CPA_REPOSITORY = "router-for-me/CLIProxyAPI"
PLUGIN = "omp-gateway-usage"
USAGE_PATH = f"/v0/management/plugins/{PLUGIN}/usage"
RESOURCE_PATH = f"/v0/resource/plugins/{PLUGIN}/status"
GATEWAY_TOKEN = "synthetic-gateway-token"
PRIVATE_SENTINEL = "private-account@example.test"
LIBRARY_EXTENSIONS = {"linux": ".so", "darwin": ".dylib", "windows": ".dll"}
ESSENTIAL_ENVIRONMENT = (
    "PATH",
    "PATHEXT",
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "WINDIR",
    "COMSPEC",
    "LANG",
    "LC_ALL",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
)
OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


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


def redact(text, values):
    for value in values:
        if value:
            text = text.replace(value, "[REDACTED]")
    return text


def fetch(base, path, token=None, timeout=15):
    request = urllib.request.Request(base + path)
    if token:
        request.add_header("Authorization", f"Bearer {token}")
    try:
        with OPENER.open(request, timeout=timeout) as response:
            return response.status, response.read(), response.headers
    except urllib.error.HTTPError as error:
        try:
            return error.code, error.read(), error.headers
        finally:
            error.close()


def detect_target():
    system = platform.system()
    if system == "Linux":
        goos = "linux"
    elif system == "Darwin":
        goos = "darwin"
    elif system == "Windows":
        goos = "windows"
    else:
        raise SystemExit(f"Unsupported operating system for native CPA smoke: {system}")
    machine = platform.machine().strip().lower()
    if machine in ("x86_64", "amd64", "x64"):
        goarch = "amd64"
    elif machine in ("aarch64", "arm64"):
        goarch = "arm64"
    else:
        raise SystemExit(f"Unsupported architecture for native CPA smoke: {machine}")
    if goos == "windows" and goarch != "amd64":
        raise SystemExit("Native CPA smoke supports only windows amd64")
    return goos, goarch


def cpa_asset_name(goos, goarch):
    asset_arch = "aarch64" if goarch == "arm64" else "amd64"
    suffix = "zip" if goos == "windows" else "tar.gz"
    return f"CLIProxyAPI_{CPA_VERSION}_{goos}_{asset_arch}.{suffix}"


def cpa_executable_name(goos):
    return "cli-proxy-api.exe" if goos == "windows" else "cli-proxy-api"




def gh_release_download(directory, asset, redactions):
    command = [
        "gh", "release", "download", CPA_TAG,
        "--repo", CPA_REPOSITORY,
        "--pattern", asset,
        "--pattern", "checksums.txt",
        "--dir", str(directory),
    ]
    try:
        result = subprocess.run(
            command,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            encoding="utf-8",
            errors="replace",
        )
    except FileNotFoundError as error:
        raise RuntimeError("The gh CLI is required to download the official CPA release") from error
    if result.returncode != 0:
        detail = redact((result.stderr or result.stdout).strip(), redactions)
        raise RuntimeError(f"gh release download failed with exit code {result.returncode}: {detail}")


def verify_cpa_checksum(directory, asset):
    checksums = directory / "checksums.txt"
    if not checksums.is_file():
        raise RuntimeError("The official CPA release checksums.txt was not downloaded")
    expected = None
    for line in checksums.read_text(encoding="utf-8").splitlines():
        fields = line.split()
        if len(fields) == 2 and fields[1].lstrip("*") == asset:
            expected = fields[0].lower()
    if expected is None or not re.fullmatch(r"[0-9a-f]{64}", expected):
        raise RuntimeError(f"The official CPA checksums.txt has no SHA-256 entry for {asset}")
    with (directory / asset).open("rb") as stream:
        actual = hashlib.file_digest(stream, "sha256").hexdigest()
    if actual != expected:
        raise RuntimeError(f"SHA-256 mismatch for the official CPA release asset {asset}")




def extract_cpa_executable(archive, executable, goos):
    expected = cpa_executable_name(goos)
    if goos == "windows":
        with zipfile.ZipFile(archive) as bundle:
            with bundle.open(expected) as source, executable.open("wb") as target:
                shutil.copyfileobj(source, target)
    else:
        with tarfile.open(archive, mode="r:gz") as bundle:
            member = bundle.getmember(expected)
            if not member.isfile():
                raise RuntimeError(f"{expected} is not a regular file in {archive.name}")
            with bundle.extractfile(member) as source, executable.open("wb") as target:
                shutil.copyfileobj(source, target)
        executable.chmod(0o755)


def extract_plugin(archive, root, goos, goarch):
    extension = LIBRARY_EXTENSIONS[goos]
    library_name = f"{PLUGIN}{extension}"
    expected = {library_name, "LICENSE", "THIRD_PARTY_NOTICES.md"}
    with zipfile.ZipFile(archive) as bundle:
        files = [info for info in bundle.infolist() if not info.is_dir()]
        names = [info.filename for info in files]
        if len(names) != len(set(names)):
            raise RuntimeError("Plugin archive contains duplicate file entries")
        if set(names) != expected:
            raise RuntimeError(
                f"Plugin archive layout mismatch: expected {sorted(expected)}, found {sorted(names)}"
            )
        library = files[names.index(library_name)]
        target = root / "plugins" / goos / goarch / library_name
        target.parent.mkdir(parents=True, exist_ok=True)
        with bundle.open(library) as source, target.open("wb") as destination:
            shutil.copyfileobj(source, destination, length=1 << 20)
    return target


def reserve_port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as reservation:
        reservation.bind(("127.0.0.1", 0))
        return reservation.getsockname()[1]


def child_environment(root, goos):
    environment = {}
    for key in ESSENTIAL_ENVIRONMENT:
        # Windows resolves the absolute executable and system DLLs without PATH. Omitting it
        # prevents an accidental MinGW/toolchain DLL from satisfying a broken plugin dependency.
        if goos == "windows" and key == "PATH":
            continue
        value = os.environ.get(key)
        if value:
            environment[key] = value
    home = root / "home"
    temporary = root / "tmp"
    home.mkdir()
    temporary.mkdir()
    environment["HOME"] = str(home)
    environment["USERPROFILE"] = str(home)
    environment["TMP"] = environment["TEMP"] = environment["TMPDIR"] = str(temporary)
    if goos == "windows":
        for key in ("APPDATA", "LOCALAPPDATA"):
            directory = root / key.lower()
            directory.mkdir()
            environment[key] = str(directory)
    return environment


def stop_process(process):
    if process.poll() is None:
        process.terminate()
    try:
        process.wait(timeout=20)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()


def wait_ready(process, base, admin):
    deadline = time.monotonic() + 45
    while time.monotonic() < deadline:
        code = process.poll()
        if code is not None:
            raise RuntimeError(f"CPA exited before readiness with code {code}")
        try:
            status, body, _ = fetch(base, "/v8/management/plugins", admin, timeout=5)
        except urllib.error.URLError:
            time.sleep(0.25)
            continue
        if status != 200:
            raise RuntimeError(f"CPA management readiness returned HTTP {status}")
        return json.loads(body)
    raise RuntimeError("CPA did not become ready within 45 seconds")


def main():
    if len(sys.argv) != 3:
        raise SystemExit("Usage: python scripts/smoke.py <platform.zip> <version>")
    archive = Path(sys.argv[1]).resolve(strict=True)
    version = sys.argv[2]
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
        raise SystemExit(f"Expected a numeric major.minor.patch version, got {version}")
    goos, goarch = detect_target()
    expected_archive = f"{PLUGIN}_{version}_{goos}_{goarch}.zip"
    if archive.name != expected_archive:
        raise SystemExit(f"Expected platform archive {expected_archive}, got {archive.name}")

    admin = secrets.token_urlsafe(32)
    caller = secrets.token_urlsafe(24)
    redactions = [
        admin, caller, GATEWAY_TOKEN, PRIVATE_SENTINEL,
        os.environ.get("GH_TOKEN"), os.environ.get("GITHUB_TOKEN"),
    ]

    with tempfile.TemporaryDirectory(prefix="cpa-omp-usage-smoke-") as temporary:
        root = Path(temporary)
        library = extract_plugin(archive, root, goos, goarch)
        cpa_dir = root / "cpa"
        cpa_dir.mkdir()
        cpa_asset = cpa_asset_name(goos, goarch)
        gh_release_download(cpa_dir, cpa_asset, redactions)
        verify_cpa_checksum(cpa_dir, cpa_asset)
        executable = cpa_dir / cpa_executable_name(goos)
        extract_cpa_executable(cpa_dir / cpa_asset, executable, goos)

        gateway = ThreadingHTTPServer(("127.0.0.1", 0), Gateway)
        gateway.daemon_threads = True
        thread = threading.Thread(target=gateway.serve_forever, daemon=True)
        thread.start()
        try:
            token = root / "gateway.token"
            token.write_text(GATEWAY_TOKEN)
            token.chmod(0o600)
            port = reserve_port()
            config = {
                "host": "127.0.0.1",
                "port": port,
                "auth-dir": str(root / "auth"),
                "remote-management": {
                    "allow-remote": False,
                    "secret-key": admin,
                    "disable-control-panel": True,
                },
                "access": {"api-keys": [caller]},
                "plugins": {
                    "enabled": True,
                    "dir": str(root / "plugins"),
                    "configs": {PLUGIN: {
                        "enabled": True,
                        "gateway_url": f"http://127.0.0.1:{gateway.server_port}",
                        "token_file": str(token),
                    }},
                },
                "logging-to-file": False,
                "debug": False,
                "usage-statistics-enabled": False,
                "discovery": {"enabled": False},
            }
            (root / "auth").mkdir()
            config_path = root / "config.yaml"
            config_path.write_text(json.dumps(config))
            config_path.chmod(0o600)
            run_dir = root / "run"
            run_dir.mkdir()
            log_path = root / "cpa.log"
            with log_path.open("wb") as log_handle:
                process = subprocess.Popen(
                    [str(executable), "-config", str(config_path)],
                    cwd=run_dir,
                    env=child_environment(root, goos),
                    stdin=subprocess.DEVNULL,
                    stdout=log_handle,
                    stderr=subprocess.STDOUT,
                )
                try:
                    base = f"http://127.0.0.1:{port}"
                    management = wait_ready(process, base, admin)
                    plugin = next(item for item in management["plugins"] if item["id"] == PLUGIN)
                    assert plugin["registered"] and plugin["effective_enabled"], "Native plugin failed registration"
                    assert plugin["metadata"]["version"] == version, "Embedded version differs from release version"

                    status, body, _ = fetch(base, RESOURCE_PATH)
                    assert status == 200 and b"OMP Gateway Usage" in body, "Static plugin resource failed"
                    assert PRIVATE_SENTINEL.encode() not in body and GATEWAY_TOKEN.encode() not in body

                    assert fetch(base, "/v1/models")[0] == 401, "Anonymous inference access was accepted"
                    status, body, _ = fetch(base, "/v1/models", caller)
                    assert status == 200, f"Inference key was rejected by /v1/models with HTTP {status}"
                    assert isinstance(json.loads(body)["data"], list), "Inference key did not receive a model list"
                    assert fetch(base, USAGE_PATH)[0] == 401, "Anonymous quota access was accepted"
                    assert fetch(base, USAGE_PATH, caller)[0] == 401, "Inference key gained management access"

                    status, body, headers = fetch(base, USAGE_PATH, admin)
                    assert status == 200, f"Authenticated quota query returned {status}"
                    assert headers.get("Cache-Control") == "no-store"
                    assert PRIVATE_SENTINEL.encode() not in body and GATEWAY_TOKEN.encode() not in body
                    limits = json.loads(body)["reports"][0]["limits"]
                    assert limits[0]["usedPercent"] == 25 and limits[0]["remainingPercent"] == 75
                    print(
                        f"PASS: CPA {CPA_VERSION} {goos}/{goarch}, native version {version}, "
                        f"quota projection, management boundary, library {library.name}"
                    )
                finally:
                    stop_process(process)
                    if sys.exc_info()[0] is not None:
                        log_handle.flush()
                        logs = log_path.read_text(encoding="utf-8", errors="replace")
                        print(redact(logs, redactions), file=sys.stderr)
        finally:
            gateway.shutdown()
            gateway.server_close()
            thread.join()


if __name__ == "__main__":
    main()
