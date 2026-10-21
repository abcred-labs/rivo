# Stable release plan (draft)

Rivo is a 0.x project: behavior, file formats and platform support may still change, and the changelog says so. This page records what must be true before a 1.0 release, how repository security is verified, and what code signing needs. It is a maintainer-owned draft; change it when the decisions change.

## Release preparation

```text
python scripts/bump_version.py <next-version>
```

The helper bumps `runtime/Cargo.toml`, refreshes `Cargo.lock` and `licenses/`, and adds `CHANGELOG.md` / `CHANGELOG.en.md` stubs that must be filled in. It never commits or tags. The Release workflow refuses a tag whose version differs from `Cargo.toml`, whose changelog entries are missing or left unfilled, or whose commit is not on `main`. Follow [the release runbook](releasing.md) for the gate and tagging.

## Repository security check

```text
GH_TOKEN=<token with repository administration read access> python scripts/check_repo_security.py
```

The script verifies the active tag ruleset, branch protection or a branch ruleset for `main`, secret scanning with push protection, Dependabot security updates, private vulnerability reporting, `dependabot.yml` and `SECURITY.md`. Without a token it reports the administration-only items as `UNKNOWN`; it exits non-zero only when a setting is verifiably missing. Run it before each release.

## Criteria for a 1.0 release (draft)

1. **Compatibility promise.** Config schema, session schema, the app-server protocol and the upgrade manifest are documented as stable, with tested upgrades from every supported 0.2.x release.
2. **Real-environment acceptance.** Real browser, interactive TUI, a real model service and cross-node A2A are covered by repeatable acceptance runs, not only mock-based smoke tests. Tests marked ignored in the default suite are either enabled or documented as out of scope.
3. **Platform support matrix.** Supported operating systems, architectures and libc versions are listed and exercised in CI; unsupported targets (for example musl/Alpine) stay explicitly excluded.
4. **Supply chain.** Archives have checksums, a CycloneDX SBOM and build-provenance attestations (done), plus platform code signing (below).
5. **Repository hygiene.** `scripts/check_repo_security.py` passes with a token, and open dependency updates are triaged.
6. **Documentation.** Install, upgrade, rollback and uninstall are documented in English and Chinese and verified on every supported platform.

## Code signing

Archives are not signed today; SHA-256 and the attestation prove integrity and origin, not publisher identity to the operating system.

| Platform | Needs | Notes |
| --- | --- | --- |
| Windows | An Authenticode certificate or Azure Trusted Signing account, stored as repository secrets | Sign `rivo.exe` and `rivo-browser-extension-host.exe` before packaging so the archive checksum covers the signed files. Removes SmartScreen warnings for downloaded ZIPs. |
| macOS | An Apple Developer ID certificate and notarization credentials | Sign the `rivo` binary with the hardened runtime and notarize it. Bare binaries cannot be stapled; files fetched with `curl` carry no quarantine flag, so the installer path works without it, but browser-downloaded archives benefit. |
| Linux | Optional detached signature (for example cosign) | The attestation already binds archives to the workflow. |

None of this can be done without credentials owned by the maintainers. Once secrets exist, add the signing step in `.github/workflows/release.yml` between "Build release binaries" and "Package archive", and extend `scripts/release_smoke.py` to assert the signature.

[Release runbook](releasing.md) | [Chinese version](stable-release-plan.zh-CN.md)
