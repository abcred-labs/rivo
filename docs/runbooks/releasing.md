# Open-source release and acceptance runbook

The public delivery is the CLI/TUI archive set. Release version, source commit, manifests and extracted binary versions must agree. A build passing in a historical report does not replace the current GitHub Actions evidence.

## Quality gate

Run from the repository root with the pinned Rust toolchain, Python 3.11+ and Node.js 22:

```text
cd runtime
cargo run -p xtask -- check
cd ..
python -m compileall -q scripts packaging
python scripts/check_markdown_links.py
python scripts/check_doc_parity.py
python scripts/check_browser_extension.py
python scripts/package-test.py
python scripts/install-command-test.py
node --test packaging/browser-extension/tests/content-script.test.mjs packaging/browser-extension/tests/service-worker.test.mjs
cargo audit --file runtime/Cargo.lock --deny warnings
python scripts/generate_licenses.py --check
```

The workflow also runs Rust 1.88 MSRV, dependency/license, Linux, Windows and macOS quality jobs. Dependency changes require refreshed `licenses/` output.

## Archive matrix

| Target | Runner | Archive |
| --- | --- | --- |
| Windows amd64 | `windows-2025` | ZIP |
| Linux amd64 | `ubuntu-22.04` | tar.gz |
| Linux arm64 | `ubuntu-22.04-arm` | tar.gz |
| macOS amd64 | `macos-15-intel` | tar.gz |
| macOS arm64 | `macos-15` | tar.gz |

Each runner builds the release binary, runs the Linux ABI check where applicable, runs `scripts/package.py`, creates the platform manifest and runs `scripts/release_smoke.py`. Linux binaries also run a Debian 12 container smoke before packaging. Archive smoke checks cover archive checksum, manifest, binary version, English/Chinese help, schema 6 empty configuration, language files, dependency notices, loopback mock Provider and upgrade manifest handling. Windows additionally checks the browser-extension host.

Release smoke does not prove every terminal emulator, real model service, real browser, or cross-node A2A deployment. Linux archives target GNU/glibc and require `libdbus-1.so.3`. They are built on Ubuntu 22.04 and smoke-tested in Debian 12, whose glibc 2.36 runtime is the minimum supported Linux baseline. Archives are unsigned.

## Draft and publish sequence

1. Update `CHANGELOG.md` with behavior, compatibility and known limitations.
2. Run the quality gate on the exact commit that will be tagged.
3. Dispatch the Release workflow with `major.minor.patch`, or push the matching `v<version>` tag. The workflow runs the full gate and the five-target matrix, then publishes the release as latest only if every job passes; a failed job publishes nothing.
4. Inspect all five archives, sidecars, manifests, smoke reports and commit fields. The workflow refuses replacing a published version.
5. Verify the installation docs, checksum commands, archive README and manual-upgrade behavior.
6. Review repository security settings: private vulnerability reporting, secret scanning/push protection, Dependabot, branch protection and immutable release tags.
7. After the workflow publishes, download each real asset again and run the documented checks plus `rivo upgrade --check`.

Versions currently use three numeric components and the manifest channel is `stable`. A 0.x release must state experimental behavior and platform limits; it must not claim production stability.

[Install guide](../guides/one-click-install.md) | [Chinese version](releasing.zh-CN.md)
