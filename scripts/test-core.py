#!/usr/bin/env python3
"""Build current core-test executables using the same verified V8 assets as CI."""

import hashlib
import os
from pathlib import Path
import subprocess
import sys


def checksum(file):
    digest = hashlib.sha256()
    with file.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def main():
    root = Path(__file__).resolve().parents[1] / "codex-rs"
    version = (
        subprocess.check_output(
            ["cargo", "pkgid", "--locked", "v8"],
            cwd=root,
            text=True,
        )
        .strip()
        .rsplit("@", 1)[1]
    )
    target = next(
        line.removeprefix("host: ")
        for line in subprocess.check_output(["rustc", "-vV"], text=True).splitlines()
        if line.startswith("host: ")
    )
    profile = f"ptrcomp_sandbox_release_{target}"
    archive = (
        f"rusty_v8_{profile}.lib.gz"
        if "windows" in target
        else f"librusty_v8_{profile}.a.gz"
    )
    binding = f"src_binding_{profile}.rs"
    manifest = f"rusty_v8_{profile}.sha256"
    cache = (
        (root / os.environ.get("CARGO_TARGET_DIR", "target")).resolve()
        / "rusty-v8"
        / version
        / target
    )
    cache.mkdir(parents=True, exist_ok=True)
    base = f"https://github.com/openai/codex/releases/download/rusty-v8-v{version}"
    if not (cache / manifest).exists():
        temporary = cache / (manifest + ".download")
        subprocess.run(
            [
                "curl",
                "-fLSs",
                "--retry",
                "3",
                f"{base}/{manifest}",
                "-o",
                str(temporary),
            ],
            check=True,
        )
        temporary.replace(cache / manifest)
    checksums = dict(
        line.split()[::-1] for line in (cache / manifest).read_text().splitlines()
    )
    if set(checksums) != {archive, binding}:
        raise RuntimeError(
            "V8 checksum manifest must contain exactly the archive and bindings"
        )
    for name, expected in checksums.items():
        file = cache / name
        if file.exists() and checksum(file) == expected:
            continue
        temporary = file.with_suffix(file.suffix + ".download")
        subprocess.run(
            ["curl", "-fLSs", "--retry", "3", f"{base}/{name}", "-o", str(temporary)],
            check=True,
        )
        if checksum(temporary) != expected:
            raise RuntimeError(f"V8 checksum mismatch for {name}")
        temporary.replace(file)
    environment = {
        **os.environ,
        "RUSTY_V8_ARCHIVE": str(cache / archive),
        "RUSTY_V8_SRC_BINDING_PATH": str(cache / binding),
    }
    environment.setdefault("CARGO_BUILD_JOBS", "2")
    # Cargo can reuse this crate after a checkout move even though its resource
    # resolver embeds CARGO_MANIFEST_DIR. Invalidate only that generated cache
    # once per checkout root, not the whole target directory on every test run.
    stamp = cache.parents[2] / "core-test-source-root"
    if not stamp.exists() or stamp.read_text() != str(root):
        subprocess.run(
            ["cargo", "clean", "-p", "codex-utils-cargo-bin", "--profile", "dev"],
            cwd=root,
            env=environment,
            check=True,
        )
        stamp.write_text(str(root))
    packages = [
        "codex-cli",
        "codex-exec",
        "codex-code-mode-host",
        "codex-rmcp-client",
        "codex-shell-escalation",
        "codex-linux-sandbox",
    ]
    if "windows" in target:
        packages.append("codex-windows-sandbox")
    subprocess.run(
        [
            "cargo",
            "build",
            "--locked",
            *[argument for package in packages for argument in ("-p", package)],
            "--bins",
        ],
        cwd=root,
        env=environment,
        check=True,
    )
    subprocess.run(
        ["just", "test", "-p", "codex-core", "--test-threads", "4", *sys.argv[1:]],
        cwd=root,
        env=environment,
        check=True,
    )


if __name__ == "__main__":
    main()
