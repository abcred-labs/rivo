# Install Rivo CLI

## Install the latest release

On Linux or macOS, run the short installer with administrator privileges:

```sh
curl -fsSL https://github.com/abcred-labs/rivo/releases/latest/download/rivo-install.sh | sudo sh
rivo --version
```

On Windows, open an elevated PowerShell and run:

```powershell
irm https://github.com/abcred-labs/rivo/releases/latest/download/rivo-install.ps1 | iex
rivo --version
```

The scripts detect the supported platform and architecture, download the matching
manifest and archive, verify both checksum sources, and install the complete
resource directory. Unix installs use `/usr/local/lib/rivo` and
`/usr/local/bin/rivo`; Windows installs use `%ProgramFiles%\Rivo\current` and
the system PATH. The scripts accept `--version`, `--install-root` and `--base-url`
(PowerShell uses the corresponding named parameters). The release archives are
unsigned, so SHA-256 verifies integrity but does not authenticate the publisher.
When a Unix `--install-root` other than `/usr/local/lib/rivo` is supplied, the
launcher is written to `<install-root>/bin/rivo` so local mirror tests do not
modify `/usr/local/bin`.

The installer scripts are published as `rivo-install.sh` and `rivo-install.ps1`
assets on every Release. The repository's `print-install-commands.py` helper only
prints manual archive commands; it does not install or modify PATH.

## Install from mainland China

Direct downloads from GitHub are often slow or interrupted on mainland China networks.
Point the installer at the download mirrors instead:

| Variable | Mirror | Replaces |
| --- | --- | --- |
| `RIVO_GITHUB_MIRROR` | `https://github-mirror.abc.red` | `https://github.com` (manifest, installer and archive URLs) |
| `RIVO_RELEASE_ASSETS_MIRROR` | `https://gh-release-assets.abc.red` | `https://release-assets.githubusercontent.com` (the redirect GitHub uses for the actual download) |

Linux or macOS. `sudo` clears the environment, so pass the variables with `sudo env`:

```sh
curl -fsSL https://github-mirror.abc.red/abcred-labs/rivo/releases/latest/download/rivo-install.sh | sudo env RIVO_GITHUB_MIRROR=https://github-mirror.abc.red RIVO_RELEASE_ASSETS_MIRROR=https://gh-release-assets.abc.red sh
rivo --version
```

Windows, from an elevated PowerShell (`-GithubMirror` and `-ReleaseAssetsMirror` are the equivalent parameters):

```powershell
$env:RIVO_GITHUB_MIRROR = 'https://github-mirror.abc.red'
$env:RIVO_RELEASE_ASSETS_MIRROR = 'https://gh-release-assets.abc.red'
irm https://github-mirror.abc.red/abcred-labs/rivo/releases/latest/download/rivo-install.ps1 | iex
rivo --version
```

The installers follow redirects one HTTPS hop at a time and rewrite each hop to the
configured mirror. Mirrors only speed up transport: the manifest and archive are still
checked against the SHA-256 values and expected names, and a tampered mirror response is
rejected. Both variables must be `https://` URLs and may be used independently. The
`--base-url` option is separate: it selects a self-hosted directory layout
(`<platform>-<arch>/release-manifest.json`) rather than a GitHub proxy.

`rivo upgrade` honors the same two variables (they are not persisted, so keep them in the
environment of the upgrade command):

```sh
sudo env RIVO_GITHUB_MIRROR=https://github-mirror.abc.red RIVO_RELEASE_ASSETS_MIRROR=https://gh-release-assets.abc.red rivo upgrade --apply
```

## Install a fixed release manually

Rivo releases contain portable archives for five targets:

| Target | Archive |
| --- | --- |
| Windows amd64 | `.zip` |
| Linux amd64 | `.tar.gz` |
| Linux arm64 | `.tar.gz` |
| macOS amd64 | `.tar.gz` |
| macOS arm64 | `.tar.gz` |

Download the archive, its `.sha256` sidecar and the matching
`release-manifest-<platform>-<arch>.json` from the [latest GitHub Release](https://github.com/abcred-labs/rivo/releases/latest).
There are no MSI, PKG or DEB assets in the current release workflow.

## Verify and extract

On Linux:

```sh
sha256sum -c rivo-<version>-linux-amd64.tar.gz.sha256
tar -xzf rivo-<version>-linux-amd64.tar.gz
cd rivo-<version>-linux-amd64
export PATH="$PWD:$PATH"
./rivo --version
```

On macOS:

```sh
shasum -a 256 -c rivo-<version>-macos-arm64.tar.gz.sha256
tar -xzf rivo-<version>-macos-arm64.tar.gz
cd rivo-<version>-macos-arm64
export PATH="$PWD:$PATH"
./rivo --version
```

On Windows PowerShell:

```powershell
(Get-FileHash .\rivo-<version>-windows-amd64.zip -Algorithm SHA256).Hash
Get-Content .\rivo-<version>-windows-amd64.zip.sha256
Expand-Archive .\rivo-<version>-windows-amd64.zip -DestinationPath .\rivo-unpacked
Set-Location .\rivo-unpacked\rivo-<version>-windows-amd64
$env:PATH = "$PWD;$env:PATH"
.\rivo.exe --version
```

Compare the calculated digest with the first field of the sidecar before running
anything. SHA-256 checks archive integrity; current archives are unsigned and the
checksum does not authenticate the publisher.

Keep the extracted directory together. It contains `LICENSE`, third-party notices,
English and Chinese language resources, and the example loopback provider. Windows
also contains the browser-extension host and extension resources. Linux archives are
GNU/glibc builds support Debian 12 (glibc 2.36) and newer compatible systems and
require `libdbus-1.so.3`; musl/Alpine is not a supported target.

For a copyable command list, run the print-only helper from the repository:

```text
python scripts/print-install-commands.py --platform linux --arch amd64 \
  --package ./rivo-<version>-linux-amd64.tar.gz --version <version>
```

The helper prints verification, extraction, temporary PATH and version commands. It
does not invoke an installer and does not modify the system PATH.

## Make PATH persistent

The commands above change PATH for the current shell. For a persistent installation,
move the extracted directory to a user-owned tools directory and add that directory
to the user's PATH using the operating system's normal environment settings. Do not
put credentials or Rivo data inside the release directory.

## Build from source

If no matching archive exists, install Rust through rustup and follow
[Source configuration and testing](../../SOURCE_CONFIGURATION_AND_TESTING.en.md):

```powershell
Set-Location runtime
cargo build --release --locked
```

The build does not contact a model service and does not require credentials.

## Upgrade and uninstall

```text
rivo upgrade --check
rivo upgrade --apply
```

`rivo upgrade --check` only compares the current version with the platform manifest.
`rivo upgrade --apply` downloads and verifies the newer archive, asks for terminal
confirmation, replaces the installation, and keeps one previous version for rollback.
Use `rivo upgrade --apply --yes` in non-interactive automation. Stop other Rivo
processes before applying an update; on Windows the command uses a short-lived
replacement helper because the current executable is locked while it runs. User
configuration, credentials and sessions are stored outside the release directory.

To uninstall, remove the extracted binary directory from PATH. Remove user data only
when you intentionally want to discard configuration, credentials, sessions and logs;
inspect their locations with `rivo config path`, `rivo config credentials path` and
`rivo sessions path` first.

[Binary runtime](../runbooks/binary-runtime.md) | [Configuration guides](configuration/README.md)

---

[简体中文](one-click-install.zh-CN.md)
