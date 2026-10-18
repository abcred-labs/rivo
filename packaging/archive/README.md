# Rivo archive guide

Release archives are published on the [GitHub Releases page](https://github.com/abcred-labs/rivo/releases). Windows amd64 uses ZIP; Linux amd64/arm64 and macOS amd64/arm64 use tar.gz. Apple Silicon users should use the native arm64 archive.

## Verify and extract

Download the archive and its .sha256 sidecar from one GitHub Release. Verify the digest before extracting:

~~~powershell
Get-FileHash .\\rivo-<version>-windows-amd64.zip -Algorithm SHA256
Expand-Archive .\\rivo-<version>-windows-amd64.zip -DestinationPath .\\rivo-unpacked
Set-Location .\\rivo-unpacked\\rivo-<version>-windows-amd64
$env:PATH = "$PWD;$env:PATH"
~~~

~~~sh
sha256sum -c rivo-<version>-linux-amd64.tar.gz.sha256
tar -xzf rivo-<version>-linux-amd64.tar.gz
cd rivo-<version>-linux-amd64
export PATH="$PWD:$PATH"
~~~

On macOS, use `shasum -a 256 -c` instead of `sha256sum -c`. Linux requires glibc and the system D-Bus library (`libdbus-1.so.3`, provided by `libdbus-1-3` on Ubuntu); musl/Alpine is not a supported archive target.

Keep the extracted directory together: it contains LICENSE, complete dependency licenses in `share/rivo/licenses/`, language files and, on Windows, the browser extension host and extension resources. Linux and macOS archives provide the CLI/TUI; Windows additionally provides Windows Terminal, Computer Use and browser-extension integrations. Archives are unsigned; checksums verify file integrity and do not provide publisher authentication.

## Local mock provider

The archive includes a deterministic local mock provider. Start it with Python 3:

~~~text
python share/rivo/examples/mock_provider.py
~~~

Then configure an isolated provider and workspace:

~~~text
rivo config vendor set mock-local --display-name "Local Mock"
rivo config model set mock-local-model --vendor mock-local --display-name "Local Mock" --protocols openai_chat_completions
rivo config provider set mock-local --base-url http://127.0.0.1:18780/v1
rivo config provider models refresh mock-local
rivo run --vendor mock-local --model mock-local-model --provider-id mock-local --prompt "Reply RIVO_MOCK_OK"
~~~

This fixture listens only on loopback and does not contact a model service.

## Credentials and data

Store provider keys through rivo config credentials set or reference an environment variable for one run. Never put keys in command arguments, configuration files, logs or reports. Configuration, credentials, SQLite history and logs remain in the user's local data directories; inspect their locations with rivo config path, rivo config credentials path and rivo sessions path.
