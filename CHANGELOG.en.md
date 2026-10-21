# Changelog (English)

Chinese is the primary changelog ([CHANGELOG.md](CHANGELOG.md)); this file mirrors each release for English readers. Release notes on GitHub include the matching entry.

## 0.2.11 (English)

### Upgrade and installer hardening

- `rivo upgrade` now requires an `https://` `RIVO_RELEASE_BASE_URL` (plain `http://` is only accepted for loopback hosts), streams downloads to disk with a 512 MiB cap, and limits manifest/checksum responses to 1 MiB.
- ZIP and tar extraction reject symbolic links and cap entry counts (10,000) and unpacked size (2 GiB). The Unix installer, the PowerShell installer and `rivo upgrade` validate archive member paths before installing, and replace an existing version directory with a rollback instead of deleting it first.
- `rivo-install.sh` / `rivo-install.ps1` apply the same `--base-url` / `-BaseUrl` rule.

### Release supply chain

- Every archive now ships with a CycloneDX SBOM (`rivo-<version>-<platform>-<arch>.sbom.cdx.json`) and a GitHub build-provenance attestation. Verify with `gh attestation verify <archive> --repo abcred-labs/rivo`. Archives are still not platform code-signed.
- The Release workflow refuses to run unless the tag matches `runtime/Cargo.toml`, `CHANGELOG.md` and `CHANGELOG.en.md` have an entry for the version, and the commit is on `main`.
- CI no longer runs the same test command in two separate steps.

## 0.2.10 (English)

### Installation

- The installers (`rivo-install.sh`, `rivo-install.ps1`) and `rivo upgrade` support mainland China download mirrors through `RIVO_GITHUB_MIRROR` (for example `https://github-mirror.abc.red`) and `RIVO_RELEASE_ASSETS_MIRROR` (for example `https://gh-release-assets.abc.red`). Redirects are rewritten hop by hop; manifest, artifact name and SHA-256 verification are unchanged. Both values must be `https://` URLs.
- The Unix installer rejects archive members with absolute or `..` paths and unpacks with `--no-same-owner`.

### Security and release

- Updated `rustls-webpki` (0.103.16) and `webpki-roots` (1.0.10) for RUSTSEC-2026-0382 and RUSTSEC-2026-0381.
- The Cargo workspace version now matches the release tag. 0.2.9 was tagged but never published (RustSec gate failed) and version tags are immutable, so there is no 0.2.9 release.

## 0.2.8 (English)

- Keeps Debian 12 (glibc 2.36) as the minimum supported Linux baseline, built on Ubuntu 22.04 and smoke-tested on Debian 12.
- Fixes the Debian 12 smoke script for ShellCheck so the release gate passes with workflow linting enabled.

## 0.2.0 (English)

First MIT-licensed open-source release: CLI/TUI, model routing, file and process tools, confirmations, history recovery, Web/MCP, memory, A2A and the local app-server; native archives for Windows amd64, Linux amd64/arm64 and macOS amd64/arm64. Config schema 6, session schema 18. Archives are unsigned; 0.x makes no production-stability claim.
