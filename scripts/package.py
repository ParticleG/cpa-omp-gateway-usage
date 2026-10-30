#!/usr/bin/env python3
"""Build and package the CPA plugin for the host Go platform."""

import os
import re
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PLUGIN = "omp-gateway-usage"
LICENSES = ("LICENSE", "THIRD_PARTY_NOTICES.md")
EXTENSIONS = {
    ("linux", "amd64"): "so",
    ("linux", "arm64"): "so",
    ("darwin", "amd64"): "dylib",
    ("darwin", "arm64"): "dylib",
    ("windows", "amd64"): "dll",
}
VERSION_RE = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+")


def error(message):
    print(message, file=sys.stderr)
    return 2


def host_platform():
    result = subprocess.run(
        ["go", "env", "GOHOSTOS", "GOHOSTARCH"],
        check=True,
        stdout=subprocess.PIPE,
        text=True,
    )
    values = result.stdout.split()
    if len(values) != 2:
        raise SystemExit(f"Unexpected go env output: {result.stdout!r}")
    return values[0], values[1]


def build_linux(version, goarch, library):
    image = f"local/cpa-omp-gateway-usage:{version}-{goarch}"
    command = [
        "docker",
        "build",
        "--platform",
        f"linux/{goarch}",
        "--tag",
        image,
        "--build-arg",
        f"VERSION={version}",
    ]
    goproxy = os.environ.get("GOPROXY")
    if goproxy:
        command.extend(["--build-arg", f"GOPROXY={goproxy}"])
    command.append(str(ROOT))
    subprocess.run(command, check=True)

    container = subprocess.run(
        ["docker", "create", image, "/unused"],
        check=True,
        stdout=subprocess.PIPE,
        text=True,
    ).stdout.strip()
    try:
        subprocess.run(
            ["docker", "cp", f"{container}:/{PLUGIN}.so", str(library)],
            check=True,
        )
    finally:
        subprocess.run(
            ["docker", "rm", container],
            check=True,
            stdout=subprocess.DEVNULL,
        )


def build_native(version, goos, goarch, library):
    env = os.environ.copy()
    env["CGO_ENABLED"] = "1"
    env["GOOS"] = goos
    env["GOARCH"] = goarch
    subprocess.run(["go", "test", "-race", "./..."], cwd=ROOT, env=env, check=True)
    subprocess.run(
        [
            "go",
            "build",
            "-trimpath",
            "-buildmode=c-shared",
            "-ldflags",
            f"-X main.pluginVersion={version}",
            "-o",
            str(library),
            ".",
        ],
        cwd=ROOT,
        env=env,
        check=True,
    )


def write_archive(version, goos, goarch, library):
    dist = ROOT / "dist"
    dist.mkdir(exist_ok=True)
    target = dist / f"{PLUGIN}_{version}_{goos}_{goarch}.zip"
    # Opening with "w" drops any entries from an older local archive.
    with zipfile.ZipFile(target, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        archive.write(library, library.name)
        for name in LICENSES:
            archive.write(ROOT / name, name)
    return target


def main():
    if len(sys.argv) != 2:
        return error("Usage: python scripts/package.py <major.minor.patch>")
    version = sys.argv[1]
    if not VERSION_RE.fullmatch(version):
        return error(
            f"Expected a numeric major.minor.patch version, got {version}"
        )

    goos, goarch = host_platform()
    extension = EXTENSIONS.get((goos, goarch))
    if extension is None:
        return error(f"Unsupported build platform: {goos}/{goarch}")

    with tempfile.TemporaryDirectory(prefix="cpa-plugin-") as directory:
        library = Path(directory) / f"{PLUGIN}.{extension}"
        if goos == "linux":
            build_linux(version, goarch, library)
        else:
            build_native(version, goos, goarch, library)
        target = write_archive(version, goos, goarch, library)

    print(f"Release asset: {target.relative_to(ROOT)}")


if __name__ == "__main__":
    sys.exit(main())
