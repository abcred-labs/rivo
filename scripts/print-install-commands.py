#!/usr/bin/env python3
"""Print copyable commands for a Rivo release archive.

The release workflow publishes ZIP/tar.gz archives.  This helper is deliberately
print-only: it never invokes a package manager or changes PATH.
"""
from __future__ import annotations

import argparse
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--platform", choices=("windows", "macos", "linux"), required=True)
    parser.add_argument("--arch", required=True)
    parser.add_argument("--package", type=Path, required=True)
    parser.add_argument("--version", required=True)
    args = parser.parse_args()

    package = args.package.resolve()
    if not package.is_file():
        raise SystemExit(f"package not found: {package}")

    expected_suffix = ".zip" if args.platform == "windows" else ".tar.gz"
    if not package.name.endswith(expected_suffix):
        raise SystemExit(
            f"{args.platform}/{args.arch} releases use {expected_suffix}; got {package.name}"
        )
    checksum = package.with_name(package.name + ".sha256")
    directory = package.name[: -len(expected_suffix)]

    if args.platform == "windows":
        commands = [
            f'(Get-FileHash "{package}" -Algorithm SHA256).Hash',
            f'Get-Content "{checksum}"',
            f'Expand-Archive "{package}" -DestinationPath .\\rivo-unpacked',
            f'Set-Location .\\rivo-unpacked\\{directory}',
            '$env:PATH = "$PWD;$env:PATH"',
            '.\\rivo.exe --version',
        ]
    elif args.platform == "macos":
        commands = [
            f'cd "{package.parent}"',
            f'shasum -a 256 -c "{checksum.name}"',
            f'tar -xzf "{package.name}"',
            f'cd "{directory}"',
            'export PATH="$PWD:$PATH"',
            './rivo --version',
        ]
    elif args.platform == "linux":
        commands = [
            f'cd "{package.parent}"',
            f'sha256sum -c "{checksum.name}"',
            f'tar -xzf "{package.name}"',
            f'cd "{directory}"',
            'export PATH="$PWD:$PATH"',
            './rivo --version',
        ]
    else:
        raise SystemExit(f"unsupported platform: {args.platform}")

    print(f"Rivo {args.version} {args.platform}/{args.arch}")
    print(f"Package: {package}")
    print("Copyable archive commands:")
    for command in commands:
        print(f"  {command}")
    print("Upgrade check (installation remains manual):")
    print("  rivo upgrade --check")
    print("The checksum output must match the first field in the .sha256 sidecar.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
