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
4. **Supply chain.** Archives have checksums, a CycloneDX SBOM and build-provenance attestations (done), with code signing explicitly not planned (below).
5. **Repository hygiene.** `scripts/check_repo_security.py` passes with a token, and open dependency updates are triaged.
6. **Documentation.** Install, upgrade, rollback and uninstall are documented in English and Chinese and verified on every supported platform.

## Code signing

Decision: Rivo archives are not code-signed on any platform (no Authenticode, Apple Developer ID/notarization or detached Linux signatures). Integrity and origin are covered by the SHA-256 sidecar, the CycloneDX SBOM and the GitHub build-provenance attestation; none of them proves publisher identity to the operating system, so Windows SmartScreen and macOS Gatekeeper may warn for archives downloaded in a browser. The installers and `rivo upgrade` download with `curl`/HTTP clients, which do not apply those quarantine checks. Revisit this only if the project distributes through channels that require signing.

[Release runbook](releasing.md) | [Chinese version](stable-release-plan.zh-CN.md)
