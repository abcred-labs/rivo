#!/usr/bin/env python3
"""Prepare a release: bump the workspace version, refresh lock files and add changelog stubs.

Usage: python scripts/bump_version.py 0.2.13 [--date 2026-10-22] [--no-cargo]

The script only edits files; it never commits, tags or pushes. Fill in the generated
CHANGELOG entries (the release workflow refuses entries that still contain the TODO marker),
review `git diff`, run the quality gate, then commit and tag `v<version>` on main.
"""
from __future__ import annotations

import argparse
import datetime
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MARKER = "TODO: fill in"
VERSION_RE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+$")


def parse(version: str) -> tuple[int, ...]:
    return tuple(int(part) for part in version.split("."))


def read(path: Path) -> str:
    with open(path, encoding="utf-8", newline="") as handle:
        return handle.read()


def write(path: Path, content: str) -> None:
    with open(path, "w", encoding="utf-8", newline="") as handle:
        handle.write(content)


def run(command: list[str], cwd: Path) -> None:
    print("+", " ".join(command))
    subprocess.run(command, cwd=cwd, check=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("version")
    parser.add_argument("--date", default=datetime.date.today().isoformat())
    parser.add_argument("--no-cargo", action="store_true", help="skip Cargo.lock and license refresh")
    args = parser.parse_args()
    version = args.version
    if not VERSION_RE.match(version):
        raise SystemExit("version must be major.minor.patch")

    cargo_toml = ROOT / "runtime" / "Cargo.toml"
    text = read(cargo_toml)
    match = re.search(r'^version = "([0-9]+\.[0-9]+\.[0-9]+)"$', text, re.M)
    if not match:
        raise SystemExit("cannot find the workspace version in runtime/Cargo.toml")
    current = match.group(1)
    if parse(version) <= parse(current):
        raise SystemExit(f"{version} must be greater than the current version {current}")

    zh_path = ROOT / "CHANGELOG.md"
    en_path = ROOT / "CHANGELOG.en.md"
    zh = read(zh_path)
    en = read(en_path)
    if f"## {version}（" in zh or f"## {version} (English)" in en:
        raise SystemExit(f"CHANGELOG already has an entry for {version}")

    write(cargo_toml, text[: match.start(1)] + version + text[match.end(1) :])
    zh_stub = f"## {version}（{args.date}）\n\n### 变更\n\n- {MARKER}\n\n"
    en_stub = f"## {version} (English)\n\n### Changes\n\n- {MARKER}\n\n"
    write(zh_path, re.sub(r"^## ", lambda m: zh_stub + m.group(0), zh, count=1, flags=re.M))
    write(en_path, re.sub(r"^## ", lambda m: en_stub + m.group(0), en, count=1, flags=re.M))

    if not args.no_cargo:
        run(["cargo", "update", "--workspace", "--offline"], ROOT / "runtime")
        run([sys.executable, "-I", "scripts/generate_licenses.py"], ROOT)
    print(f"\nBumped {current} -> {version}. Next steps:")
    print(f"  1. Replace the '{MARKER}' lines in CHANGELOG.md and CHANGELOG.en.md")
    print("  2. cd runtime && cargo run -p xtask -- check   (plus the Python gate in docs/runbooks/releasing.md)")
    print(f"  3. git commit, push to main, then: git tag -a v{version} -m \"Rivo {version}\" && git push origin v{version}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
