# Binary runtime runbook

## Obtain a release archive

Download the archive, `.sha256` sidecar and matching manifest from [GitHub Releases](https://github.com/abcred-labs/rivo/releases). Supported targets are Windows amd64 ZIP, Linux amd64/arm64 tar.gz, and macOS amd64/arm64 tar.gz. Verify the checksum before extraction and keep the extracted directory intact.

```sh
sha256sum -c rivo-<version>-linux-amd64.tar.gz.sha256
tar -xzf rivo-<version>-linux-amd64.tar.gz
```

Use `shasum -a 256 -c` on macOS and PowerShell `Get-FileHash` on Windows. Linux archives support Debian 12 (glibc 2.36) and newer compatible GNU/glibc systems, and require `libdbus-1.so.3`; the release is not a musl/Alpine package.

## First checks

```text
rivo --version
rivo --help
rivo config --help
rivo config path
rivo config credentials path
rivo sessions path
```

Configure a Credential, Vendor, Model and Provider as described in the
[configuration guides](../guides/configuration/README.md). Run
`rivo config provider models refresh <provider-id>` before a real task.

## Run CLI and TUI

Use `rivo run --prompt <TEXT>` or `rivo run --stdin` for headless work. Use
`rivo tui [--workspace <PATH>]` only from an interactive terminal. TUI keyboard and
slash commands are documented in the [TUI guide](../guides/tui.md).

## Upgrade checks

```text
rivo upgrade --check
```

`--check` reads the current platform manifest and reports whether a newer archive
exists; it never downloads or installs anything. `rivo upgrade --apply` (add `--yes`
for non-interactive use) downloads the archive, verifies the manifest and SHA-256,
atomically replaces the installation and keeps one previous version. Stop other Rivo
processes first.

`RIVO_RELEASE_BASE_URL` can point to a self-hosted mirror that exposes
`<platform>-<arch>/release-manifest.json`. On networks where GitHub is slow, set
`RIVO_GITHUB_MIRROR` and `RIVO_RELEASE_ASSETS_MIRROR` (both `https://` URLs, for example
`https://github-mirror.abc.red` and `https://gh-release-assets.abc.red`) to rewrite
GitHub and release-asset redirects; checksum verification is unchanged. See the
[install guide](../guides/one-click-install.md#install-from-mainland-china).

For a manual upgrade, stop Rivo processes, download and verify the new archive, replace the
old extracted binary directory, and run `rivo --version`. Published versions and
archives are immutable; publish a new patch version for a correction.

## Local data and diagnostics

Configuration and Credential files are outside the archive. Use `config path`,
`config credentials path`, and `sessions path` before backing up or removing data.
For a redacted diagnostic bundle:

```text
rivo diagnostics create <destination-path> --yes
```

Treat diagnostics and session backups as sensitive because they may contain redacted
business content. Never include API keys in a report.

[Install guide](../guides/one-click-install.md) | [TUI guide](../guides/tui.md) |
[Configuration guides](../guides/configuration/README.md)

---

[简体中文](binary-runtime.zh-CN.md)
