# Rivo Browser Extension (Chrome / Edge)

The Windows release archive contains the native messaging host and the extension resources under `share/rivo/browser-extension/`. The extension is a Windows-only integration; Linux and macOS archives do not include it.

## Load the extension for development

1. Extract the matching Windows amd64 archive and keep the directory intact.
2. Open `chrome://extensions` or `edge://extensions`.
3. Enable Developer mode and choose **Load unpacked**.
4. Select the extracted `share/rivo/browser-extension` directory.
5. Run the packaged `install-native-host.ps1` from an elevated PowerShell only when the browser profile and native-host path are correct.
6. Restart the browser and verify the extension can reach `rivo-browser-extension-host.exe`.

The native-host manifest is installed for the current user and points to the extracted host. Do not move or delete the archive directory while the extension is enabled; reinstall the host after moving it.

## Security and permissions

The content script and service worker communicate through the browser extension boundary and the local native messaging host. The host must validate message shape, origin/profile and EOF, and must not receive Provider Credentials or arbitrary process-control requests. Browser page content is not automatically copied into ordinary Session history.

Use a dedicated browser profile with loopback CDP when Browser tool access is also enabled. The extension does not grant `--browser`, network or MCP permissions to a Turn.

## Test

From the repository root:

```text
python scripts/check_browser_extension.py
node --test packaging/browser-extension/tests/content-script.test.mjs packaging/browser-extension/tests/service-worker.test.mjs
```

[Chinese version](README.zh-CN.md)
