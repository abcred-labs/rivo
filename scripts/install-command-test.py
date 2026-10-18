#!/usr/bin/env python3
"""Regression checks for the archive command printer."""
from __future__ import annotations

import subprocess
import sys
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "print-install-commands.py"


def run(platform: str, filename: str) -> str:
    with tempfile.TemporaryDirectory(prefix="rivo-install-command-") as directory:
        package = Path(directory) / filename
        package.write_bytes(b"archive")
        package.with_name(package.name + ".sha256").write_text(
            "0" * 64 + "  " + package.name + "\n", encoding="utf-8"
        )
        result = subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--platform",
                platform,
                "--arch",
                "amd64",
                "--package",
                str(package),
                "--version",
                "0.2.0",
            ],
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
        )
        return result.stdout


def main() -> int:
    outputs = {
        "windows": run("windows", "rivo-0.2.0-windows-amd64.zip"),
        "linux": run("linux", "rivo-0.2.0-linux-amd64.tar.gz"),
        "macos": run("macos", "rivo-0.2.0-macos-amd64.tar.gz"),
    }
    assert "msiexec" not in outputs["windows"]
    assert "installer -pkg" not in outputs["macos"]
    assert "dpkg -i" not in outputs["linux"]
    assert "Expand-Archive" in outputs["windows"]
    assert "sha256sum -c" in outputs["linux"]
    assert "shasum -a 256 -c" in outputs["macos"]
    assert 'cd "' in outputs["linux"]
    assert 'cd "' in outputs["macos"]
    for output in outputs.values():
        assert "rivo upgrade --check" in output
        assert "installation remains manual" in output
    print("archive install command checks passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
