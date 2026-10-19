# Rivo archive guide

Rivo publishes portable release archives on the [GitHub Releases page](https://github.com/abcred-labs/rivo/releases): Windows amd64 uses ZIP; Linux amd64/arm64 and macOS amd64/arm64 use tar.gz. The current release does not publish MSI, PKG or DEB packages.

## Verify and extract

Download the archive and its `.sha256` sidecar from one release. Verify before extracting:

```powershell
(Get-FileHash .\rivo-<version>-windows-amd64.zip -Algorithm SHA256).Hash
Get-Content .\rivo-<version>-windows-amd64.zip.sha256
Expand-Archive .\rivo-<version>-windows-amd64.zip -DestinationPath .\rivo-unpacked
Set-Location .\rivo-unpacked\rivo-<version>-windows-amd64
$env:PATH = "$PWD;$env:PATH"
.\rivo.exe --version
```

```sh
sha256sum -c rivo-<version>-linux-amd64.tar.gz.sha256
tar -xzf rivo-<version>-linux-amd64.tar.gz
cd rivo-<version>-linux-amd64
export PATH="$PWD:$PATH"
./rivo --version
```

Use `shasum -a 256 -c` on macOS. Linux archives support Debian 12 (glibc 2.36) and newer compatible GNU/glibc systems, and require `libdbus-1.so.3`; musl/Alpine is not a supported archive target. Archives are unsigned; the checksum verifies integrity only.

Keep the extracted directory intact. It contains the binary, license files, language
resources and `share/rivo/examples/mock_provider.py`. Windows additionally includes
the browser-extension host and resources.

## Local mock Provider

The archive includes a deterministic loopback Provider. Start it with Python 3:

```text
python share/rivo/examples/mock_provider.py
```

In another terminal configure a complete catalog and route:

```text
rivo config vendor set mock-local --display-name "Local Mock"
rivo config model set mock-local-model --vendor mock-local --display-name "Local Mock" --protocols openai_chat_completions
rivo config provider set mock-local --base-url http://127.0.0.1:18780/v1
rivo config provider models refresh mock-local
rivo config route --vendor mock-local --model mock-local-model --provider-id mock-local
rivo run --vendor mock-local --model mock-local-model --provider-id mock-local --prompt "Reply RIVO_MOCK_OK"
```

The fixture listens only on loopback and does not contact an external model service.

## Credentials and data

Store secrets with `rivo config credentials set <credential-name>`, or use `rivo run --api-key-env ENV`
for a single run. Never place a key in an argument, configuration file, log, report,
or archive. Inspect local paths with:

```text
rivo config path
rivo config credentials path
rivo sessions path
```

[Install guide](../../docs/guides/one-click-install.md) | [Configuration guides](../../docs/guides/configuration/README.md)

---

[简体中文](README.zh-CN.md)
