#!/usr/bin/env python3
"""Static and generated-output checks for the release installers."""
from __future__ import annotations

import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


GOOD_MANIFEST = """{
  "schema": 2,
  "product": "rivo",
  "version": "0.2.11",
  "platform": "linux",
  "arch": "amd64",
  "channel": "stable",
  "artifacts": [
    {
      "file": "rivo-0.2.11-linux-amd64.tar.gz",
      "sha256": "%s",
      "bytes": 12695060
    }
  ]
}
""" % ("a" * 64)


def check_manifest_parser(shell: str) -> None:
    """Run the installer's real manifest parsing functions under sh."""
    sh = shutil.which("sh")
    if sh is None:
        return
    start = shell.index("manifest_line() {")
    end = shell.index("MANIFEST_SCHEMA=$(")
    functions = shell[start:end]
    cases = {
        "good": (GOOD_MANIFEST, True),
        "minified": (json.dumps(json.loads(GOOD_MANIFEST)), False),
        "duplicate-version": (GOOD_MANIFEST.replace('"channel"', '"version": "9.9.9",\n  "channel"'), False),
        "injected-value": (GOOD_MANIFEST.replace('"linux"', '"linux; rm -rf /"'), False),
        "missing-product": (GOOD_MANIFEST.replace('  "product": "rivo",\n', ""), False),
        "nested-extra-file": (GOOD_MANIFEST.replace('"bytes"', '"file": "evil",\n      "bytes"'), False),
    }
    with tempfile.TemporaryDirectory(prefix="rivo-manifest-") as directory:
        base = Path(directory)
        for name, (body, ok) in cases.items():
            (base / "manifest.json").write_text(body, encoding="utf-8", newline="\n")
            script = (
                f'TMP_DIR="{base.as_posix()}"\n{functions}\n'
                'set -e\nfield product >/dev/null\nfield version >/dev/null\nfield platform >/dev/null\n'
                'field file >/dev/null\nfield sha256 >/dev/null\nmanifest_number schema >/dev/null\n'
            )
            result = subprocess.run([sh, "-c", script], capture_output=True, text=True)
            assert (result.returncode == 0) == ok, f"manifest case {name}: {result.stderr}"


def main() -> int:
    shell = (ROOT / "scripts" / "rivo-install.sh").read_text(encoding="utf-8")
    powershell = (ROOT / "scripts" / "rivo-install.ps1").read_text(encoding="utf-8")
    assert "--version" in shell and "--install-root" in shell and "--base-url" in shell
    assert "sha256sum" in shell and "shasum -a 256" in shell
    assert "release-manifest" in shell and "share/rivo/languages/en-US.ftl" in shell
    assert "Expand-Archive" in powershell and "Get-FileHash" in powershell
    assert "ProgramFiles" in powershell and "SetEnvironmentVariable('Path'" in powershell
    assert "rivo.exe" in powershell and "release-manifest" in powershell
    assert powershell.count("Move-Item -LiteralPath $versionDir -Destination $current") == 1
    assert "Release manifest must contain exactly one artifact." in powershell
    assert "Release checksum sidecar is empty." in powershell
    assert "print-install-commands" not in shell
    for text in (shell, powershell):
        assert "RIVO_GITHUB_MIRROR" in text and "RIVO_RELEASE_ASSETS_MIRROR" in text
        assert "release-assets.githubusercontent.com" in text
    assert "--no-same-owner" in shell and "unsafe path" in shell
    assert "must be an https:// URL (http is only allowed for loopback)" in shell
    assert "http is only allowed for loopback" in powershell
    assert "AllowAutoRedirect = $false" in powershell and "Invoke-WebRequest" not in powershell

    check_manifest_parser(shell)

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
        assert "github-mirror.abc.red" in body and "gh-release-assets.abc.red" in body
        assert "gh attestation verify" in body
        assert "rivo upgrade --check" in body
        assert "rivo upgrade --apply --yes" in body

    print("installer and release notes checks passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
