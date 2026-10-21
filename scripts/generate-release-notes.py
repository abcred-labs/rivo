#!/usr/bin/env python3
"""Generate the install and upgrade section appended to a GitHub Release."""
from __future__ import annotations

import argparse
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--changelog", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    version = args.version
    repository = args.repository
    release = f"https://github.com/{repository}/releases/download/v{version}"
    latest = f"https://github.com/{repository}/releases/latest/download"
    raw = f"https://raw.githubusercontent.com/{repository}/v{version}"
    lines = [args.changelog.read_text(encoding="utf-8").rstrip(), "", "## Install Rivo", ""]
    lines.extend(
        [
            "The release assets are unsigned. Verify the SHA-256 sidecar before running an archive.",
            "The short installers verify the release manifest and archive checksum before replacing an installation.",
            "",
            "### Fixed release assets",
            "",
            "| Platform | Archive | SHA-256 sidecar |",
            "| --- | --- | --- |",
            *[
                f"| {platform}/{arch} | [{archive}]({release}/{archive}) | [{archive}.sha256]({release}/{archive}.sha256) |"
                for platform, arch, suffix in (
                    ("Linux", "amd64", ".tar.gz"),
                    ("Linux", "arm64", ".tar.gz"),
                    ("macOS", "amd64", ".tar.gz"),
                    ("macOS", "arm64", ".tar.gz"),
                    ("Windows", "amd64", ".zip"),
                )
                for archive in (f"rivo-{version}-{platform.lower()}-{arch}{suffix}",)
            ],
            "",
            "### Linux amd64/arm64",
            "",
            f"```sh\ncurl -fsSL {release}/rivo-install.sh | sudo sh -s -- --version {version}\nrivo --version\n```",
            "",
            "### macOS amd64/arm64",
            "",
            f"```sh\ncurl -fsSL {release}/rivo-install.sh | sudo sh -s -- --version {version}\nrivo --version\n```",
            "",
            "### Windows amd64 PowerShell",
            "",
            f"```powershell\n$env:RIVO_INSTALL_VERSION = '{version}'\nirm {release}/rivo-install.ps1 | iex\nrivo --version\n```",
            "",
            "The generic latest-version commands are:",
            "",
            f"```sh\ncurl -fsSL {latest}/rivo-install.sh | sudo sh\n```",
            f"```powershell\nirm {latest}/rivo-install.ps1 | iex\n```",
            "",
            "### Mainland China mirror",
            "",
            "Set the download mirrors so the installer does not depend on direct GitHub access. Checksums are still verified against the GitHub release:",
            "",
            f"```sh\ncurl -fsSL https://github-mirror.abc.red/{repository}/releases/latest/download/rivo-install.sh | sudo env RIVO_GITHUB_MIRROR=https://github-mirror.abc.red RIVO_RELEASE_ASSETS_MIRROR=https://gh-release-assets.abc.red sh\n```",
            f"```powershell\n$env:RIVO_GITHUB_MIRROR = 'https://github-mirror.abc.red'\n$env:RIVO_RELEASE_ASSETS_MIRROR = 'https://gh-release-assets.abc.red'\nirm https://github-mirror.abc.red/{repository}/releases/latest/download/rivo-install.ps1 | iex\n```",
            "",
            "The pinned installer sources are available at:",
            f"- [{version} install.sh]({raw}/scripts/rivo-install.sh)",
            f"- [{version} install.ps1]({raw}/scripts/rivo-install.ps1)",
            "",
            "## Upgrade",
            "",
            "Check for a newer platform release:",
            "",
            "```text\nrivo upgrade --check\n```",
            "",
            "Apply an available update after confirmation:",
            "",
            "```text\nrivo upgrade --apply\nrivo upgrade --apply --yes\n```",
            "",
            "`--apply` verifies the manifest and archive, preserves one previous installation, and refuses to continue when another Rivo process is running. The system installation requires administrator privileges.",
            "",
            f"See the [installation guide](https://github.com/{repository}/blob/v{version}/docs/guides/one-click-install.md) for platform details and troubleshooting.",
        ]
    )
    args.output.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
