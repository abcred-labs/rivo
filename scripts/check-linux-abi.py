#!/usr/bin/env python3
"""Check that Linux ELF binaries do not require a newer glibc than promised."""
from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path


GLIBC_SYMBOL = re.compile(r"GLIBC_(\d+)\.(\d+)")


def parse_version(value: str) -> tuple[int, int]:
    match = re.fullmatch(r"(\d+)\.(\d+)", value)
    if match is None:
        raise argparse.ArgumentTypeError("expected VERSION as MAJOR.MINOR")
    return int(match.group(1)), int(match.group(2))


def check_binary(binary: Path, maximum: tuple[int, int]) -> int:
    if not binary.is_file():
        print(f"ABI check: binary not found: {binary}", file=sys.stderr)
        return 1
    try:
        result = subprocess.run(
            ["readelf", "--version-info", str(binary)],
            check=False,
            capture_output=True,
            text=True,
        )
    except OSError as error:
        print(f"ABI check: unable to execute readelf: {error}", file=sys.stderr)
        return 1
    if result.returncode != 0:
        print(f"ABI check failed for {binary}: {result.stderr.strip()}", file=sys.stderr)
        return 1

    versions = {
        (int(major), int(minor))
        for major, minor in GLIBC_SYMBOL.findall(result.stdout + result.stderr)
    }
    if not versions:
        print(f"ABI check failed for {binary}: no GLIBC version requirements found", file=sys.stderr)
        return 1
    highest = max(versions)
    if highest > maximum:
        required = f"{highest[0]}.{highest[1]}"
        allowed = f"{maximum[0]}.{maximum[1]}"
        print(
            f"ABI check failed for {binary}: requires GLIBC {required}, maximum is {allowed}",
            file=sys.stderr,
        )
        return 1
    print(
        f"Linux ABI OK: {binary} requires at most GLIBC {highest[0]}.{highest[1]} "
        f"(limit {maximum[0]}.{maximum[1]})"
    )
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path, nargs="+")
    parser.add_argument("--max-glibc", type=parse_version, default=(2, 36))
    args = parser.parse_args()
    return max(check_binary(binary, args.max_glibc) for binary in args.binaries)


if __name__ == "__main__":
    raise SystemExit(main())
