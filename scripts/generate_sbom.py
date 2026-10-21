#!/usr/bin/env python3
"""Generate a CycloneDX 1.5 SBOM for the Rust workspace from Cargo metadata and Cargo.lock."""
from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
LOCK = ROOT / "runtime" / "Cargo.lock"


def lock_checksums() -> dict[tuple[str, str], str]:
    checksums: dict[tuple[str, str], str] = {}
    name = version = None
    for line in LOCK.read_text(encoding="utf-8").splitlines():
        if line.startswith("[[package]]"):
            name = version = None
        elif match := re.match(r'name = "(.*)"', line):
            name = match.group(1)
        elif match := re.match(r'version = "(.*)"', line):
            version = match.group(1)
        elif match := re.match(r'checksum = "([0-9a-f]{64})"', line):
            if name and version:
                checksums[(name, version)] = match.group(1)
    return checksums


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    metadata = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--locked"],
            cwd=ROOT / "runtime",
            check=True,
            capture_output=True,
            text=True,
            encoding="utf-8",
        ).stdout
    )
    members = set(metadata["workspace_members"])
    checksums = lock_checksums()
    components = []
    for package in sorted(metadata["packages"], key=lambda item: (item["name"], item["version"])):
        if package["id"] in members:
            continue
        component = {
            "type": "library",
            "bom-ref": f"pkg:cargo/{package['name']}@{package['version']}",
            "name": package["name"],
            "version": package["version"],
            "purl": f"pkg:cargo/{package['name']}@{package['version']}",
        }
        if package.get("license"):
            component["licenses"] = [{"expression": package["license"]}]
        digest = checksums.get((package["name"], package["version"]))
        if digest:
            component["hashes"] = [{"alg": "SHA-256", "content": digest}]
        components.append(component)
    if not components:
        print("SBOM would contain no components", file=sys.stderr)
        return 1
    document = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "metadata": {
            "component": {
                "type": "application",
                "name": "rivo",
                "version": args.version,
                "properties": [{"name": "git.commit", "value": args.commit}],
            }
        },
        "components": components,
    }
    args.output.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
    print(f"wrote {args.output} ({len(components)} components)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
