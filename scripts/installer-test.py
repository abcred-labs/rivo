#!/usr/bin/env python3
"""Static and generated-output checks for the release installers."""
from __future__ import annotations

import subprocess
import sys
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


def main() -> int:
    shell = (ROOT / "scripts" / "rivo-install.sh").read_text(encoding="utf-8")
    powershell = (ROOT / "scripts" / "rivo-install.ps1").read_text(encoding="utf-8")
    assert "--version" in shell and "--install-root" in shell and "--base-url" in shell
    assert "sha256sum" in shell and "shasum -a 256" in shell
    assert "release-manifest" in shell and "share/rivo/languages/en-US.ftl" in shell
    assert "Expand-Archive" in powershell and "Get-FileHash" in powershell
    assert "ProgramFiles" in powershell and "SetEnvironmentVariable('Path'" in powershell
    assert "rivo.exe" in powershell and "release-manifest" in powershell
    assert "print-install-commands" not in shell

    with tempfile.TemporaryDirectory(prefix="rivo-release-notes-") as directory:
        output = Path(directory) / "release-body.md"
        subprocess.run(
            [
                sys.executable,
                str(ROOT / "scripts" / "generate-release-notes.py"),
                "--version",
                "0.2.0",
                "--repository",
                "abcred-labs/rivo",
                "--changelog",
                str(ROOT / "CHANGELOG.md"),
                "--output",
                str(output),
            ],
            cwd=ROOT,
            check=True,
        )
        body = output.read_text(encoding="utf-8")
        body_lower = body.lower()
        for target in ("linux amd64/arm64", "macos amd64/arm64", "windows amd64"):
            assert target in body_lower
        assert "releases/latest/download/rivo-install.sh" in body
        assert "releases/download/v0.2.0/rivo-install.ps1" in body
        assert "rivo-0.2.0-linux-amd64.tar.gz" in body
        assert "rivo-0.2.0-linux-arm64.tar.gz.sha256" in body
        assert "rivo-0.2.0-macos-amd64.tar.gz" in body
        assert "rivo-0.2.0-macos-arm64.tar.gz.sha256" in body
        assert "rivo-0.2.0-windows-amd64.zip" in body
        assert "rivo upgrade --check" in body
        assert "rivo upgrade --apply --yes" in body

    print("installer and release notes checks passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
