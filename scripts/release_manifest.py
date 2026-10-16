#!/usr/bin/env python3
"""Create the platform manifest used by GitHub Releases and `rivo upgrade`."""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path


def digest(path: Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--platform", choices=("windows", "linux", "macos"), required=True)
    parser.add_argument("--arch", required=True)
    parser.add_argument("--commit", required=True)
    args = parser.parse_args()

    prefix = f"rivo-{args.version}-{args.platform}-{args.arch}"
    archives = sorted(args.directory.glob(f"{prefix}.zip")) + sorted(args.directory.glob(f"{prefix}.tar.gz"))
    if len(archives) != 1:
        raise SystemExit(f"expected one archive matching {prefix}, found {len(archives)}")
    archive = archives[0]
    sidecar = archive.with_name(archive.name + ".sha256")
    if not sidecar.is_file():
        raise SystemExit(f"missing checksum sidecar: {sidecar}")
    actual = digest(archive)
    fields = sidecar.read_text(encoding="utf-8").split()
    if fields != [actual, archive.name]:
        raise SystemExit(f"checksum sidecar does not match {archive.name}")

    manifest = {
        "schema": 2,
        "product": "rivo",
        "version": args.version,
        "platform": args.platform,
        "arch": args.arch,
        "commit": args.commit.lower(),
        "channel": "stable",
        "signature": "unsigned",
        "artifacts": [{"file": archive.name, "sha256": actual, "bytes": archive.stat().st_size}],
    }
    output = args.directory / f"release-manifest-{args.platform}-{args.arch}.json"
    output.write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
