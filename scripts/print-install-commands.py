#!/usr/bin/env python3
"""Print copyable installation commands for a native Rivo package."""
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

    if args.platform == "windows":
        command = f'msiexec /i "{package}"'
    elif args.platform == "macos":
        command = f'sudo installer -pkg "{package}" -target /'
    elif args.platform == "linux":
        command = f'sudo dpkg -i "{package}"'
    else:
        raise SystemExit(f"unsupported platform: {args.platform}")

    print(f"Rivo {args.version} {args.platform}/{args.arch}")
    print(f"Package: {package}")
    print("Copyable install command:")
    print(command)
    print("Upgrade commands:")
    print("  rivo upgrade --check")
    print("  rivo upgrade")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
