"""生成可再分发的依赖清单与许可证正文，不保留扫描器的本机路径。"""
from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import tempfile
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
TOOL_VERSION = "0.9.2"
NATIVE_CRATES = {"aws-lc-sys", "ring", "libsqlite3-sys"}


def generate(tool: str) -> dict[str, str]:
    version = subprocess.check_output([tool, "--version"], text=True).strip()
    if version != f"cargo-about {TOOL_VERSION}":
        raise SystemExit(f"需要 cargo-about {TOOL_VERSION}，实际为 {version}")
    with tempfile.TemporaryDirectory(prefix="rivo-licenses-") as directory:
        raw = Path(directory) / "about.json"
        subprocess.run(
            [tool, "generate", "--locked", "--workspace", "--fail", "--format", "json",
             "-m", str(ROOT / "runtime/Cargo.toml"), "-o", str(raw)],
            cwd=ROOT, check=True,
        )
        data = json.loads(raw.read_text(encoding="utf-8"))

    selected: dict[tuple[str, str], set[str]] = {}
    sections = []
    for license_ in data["licenses"]:
        users = sorted({(u["crate"]["name"], u["crate"]["version"])
                        for u in license_["used_by"]})
        for key in users:
            selected.setdefault(key, set()).add(license_["id"])
        if not license_["text"].strip():
            raise SystemExit(f"许可证正文为空：{license_['id']}")
        sections.append((license_["id"], users, license_["text"].strip()))

    packages = []
    native_notices = []
    for entry in data["crates"]:
        package = entry["package"]
        if not package["source"]:
            continue
        name, version = package["name"], package["version"]
        key = (name, version)
        if not selected.get(key):
            raise SystemExit(f"依赖未覆盖许可证正文：{name} {version}")
        if not package["source"].startswith("registry+"):
            raise SystemExit(f"需要单独登记非 registry 依赖的来源：{name}")
        packages.append({
            "name": name, "version": version, "license_expression": package["license"],
            "selected_licenses": sorted(selected[key]), "authors": package["authors"],
            "repository": package["repository"],
            "source": f"https://crates.io/api/v1/crates/{name}/{version}/download",
        })
        if name not in NATIVE_CRATES:
            continue
        source = Path(package["manifest_path"]).parent
        for path in sorted(source.rglob("*")):
            if not path.is_file():
                continue
            lower = path.name.lower()
            if not any(lower.startswith(prefix) for prefix in
                       ("license", "licence", "notice", "copyright", "copying")):
                continue
            native_notices.append((f"{name}-{version}/{path.relative_to(source).as_posix()}",
                                   path.read_text(encoding="utf-8")))
        if name == "libsqlite3-sys":
            sqlite = source / "sqlite3/sqlite3.c"
            match = re.search(r"/\*\n\*\* 2001.*?\*/", sqlite.read_text(encoding="utf-8")[:4096], re.DOTALL)
            if not match or "disclaims copyright" not in match[0]:
                raise SystemExit("SQLite public-domain 声明需要重新核验")
            native_notices.append((f"{name}-{version}/sqlite3/sqlite3.c (copyright disclaimer)", match[0]))

    config = tomllib.loads((ROOT / "runtime/about.toml").read_text(encoding="utf-8"))
    inventory = {
        "schema": 1, "generator": f"cargo-about {TOOL_VERSION}",
        "cargo_lock_sha256": hashlib.sha256((ROOT / "runtime/Cargo.lock").read_bytes()).hexdigest(),
        "targets": config["targets"],
        "scope": "workspace union for release targets; includes build and dev dependencies",
        "packages": sorted(packages, key=lambda p: (p["name"], p["version"])),
    }
    text = ["Rivo third-party licenses", "=========================", "",
            "This is a conservative union for all published targets, including build/dev dependencies.",
            "A listed package is not necessarily linked into every binary.",
            "Exact, unmodified upstream source downloads are listed in dependencies.json.",
            "MPL-2.0 covered source is available from those versioned source downloads.", ""]
    for identifier, users, body in sorted(sections, key=lambda s: (s[0], s[1], s[2])):
        text.extend(["=" * 78, identifier,
                     "Used by: " + ", ".join(f"{n} {v}" for n, v in users), "", body, ""])
    for path, body in sorted(native_notices):
        text.extend(["=" * 78, f"Bundled native notice: {path}", "", body.strip(), ""])
    return {
        "dependencies.json": json.dumps(inventory, ensure_ascii=False, indent=2) + "\n",
        "THIRD_PARTY_LICENSES.txt": "\n".join(
            line.rstrip() for line in "\n".join(text).replace("\r\n", "\n").split("\n")
        ),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tool", default="cargo-about")
    parser.add_argument("--check", action="store_true", help="重新生成并拒绝过期的声明")
    args = parser.parse_args()
    output = ROOT / "licenses"
    files = generate(args.tool)
    if not args.check:
        output.mkdir(exist_ok=True)
    for name, content in files.items():
        path = output / name
        if args.check:
            if not path.is_file() or path.read_text(encoding="utf-8") != content:
                raise SystemExit(f"依赖声明过期，请运行 python scripts/generate_licenses.py：{name}")
        else:
            path.write_text(content, encoding="utf-8", newline="\n")
    print("dependency licenses: OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
