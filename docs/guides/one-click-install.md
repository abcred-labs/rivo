# Install Rivo CLI

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
GNU/glibc builds and require `libdbus-1.so.3`; musl/Alpine is not a supported target.

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
```

`rivo upgrade` only compares the current version with the platform manifest. The V1
CLI does not download, install, replace, or restart itself. Download and verify the
new archive, stop running Rivo processes, replace the old binary directory, and run
`rivo --version` again. User configuration, credentials and sessions are stored outside
the release directory and are not included in an archive.

To uninstall, remove the extracted binary directory from PATH. Remove user data only
when you intentionally want to discard configuration, credentials, sessions and logs;
inspect their locations with `rivo config path`, `rivo config credentials path` and
`rivo sessions path` first.

[Binary runtime](../runbooks/binary-runtime.md) | [Configuration guides](configuration/README.md)

---

[简体中文](one-click-install.zh-CN.md)
