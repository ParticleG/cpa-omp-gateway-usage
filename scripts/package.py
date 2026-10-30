#!/usr/bin/env python3
"""Build and package the CPA plugin for the native host platform."""

import os
import platform
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib
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
    system = platform.system().lower()
    machine = platform.machine().strip().lower()
    if machine in ("x86_64", "amd64", "x64"):
        return system, "amd64"
    if machine in ("aarch64", "arm64"):
        return system, "arm64"
    raise SystemExit(f"Unsupported native architecture: {machine}")


def build_linux(version, goarch, library):
    image = f"local/cpa-omp-gateway-usage:{version}-{goarch}"
    with (ROOT / "rust-toolchain.toml").open("rb") as handle:
        compiler = tomllib.load(handle)["toolchain"]["channel"]
    command = [
        "docker",
        "build",
        "--platform",
        f"linux/{goarch}",
        "--tag",
        image,
        "--build-arg",
        f"VERSION={version}",
        "--build-arg",
        f"RUST_IMAGE=rust:{compiler}-bookworm",
    ]
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
    arch = "x86_64" if goarch == "amd64" else "aarch64"
    abi = {
        "linux": "unknown-linux-gnu",
        "darwin": "apple-darwin",
        "windows": "pc-windows-msvc",
    }[goos]
    target = f"{arch}-{abi}"
    target_dir = ROOT / "target"
    env = os.environ.copy()
    env["PLUGIN_VERSION"] = version
    if "CARGO_ENCODED_RUSTFLAGS" in env:
        flags = [value for value in env["CARGO_ENCODED_RUSTFLAGS"].split("\x1f") if value]
    else:
        flags = env.get("RUSTFLAGS", "").split()
    flags.extend([
        f"--remap-path-prefix={Path.home()}=/build-home",
        f"--remap-path-prefix={ROOT}=.",
    ])
    if goos == "windows":
        flags.extend(["-C", "target-feature=+crt-static"])
    env["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(flags)
    env.pop("RUSTFLAGS", None)
    options = ["--locked", "--target", target, "--target-dir", str(target_dir)]
    subprocess.run(["cargo", "test", *options], cwd=ROOT, env=env, check=True)
    subprocess.run(
        ["cargo", "build", "--release", *options],
        cwd=ROOT,
        env=env,
        check=True,
    )
    prefix = "" if goos == "windows" else "lib"
    built = (
        target_dir / target / "release"
        / f"{prefix}omp_gateway_usage.{EXTENSIONS[(goos, goarch)]}"
    )
    shutil.copy2(built, library)


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
