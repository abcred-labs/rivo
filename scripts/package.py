"""构建发布归档、SHA-256 清单和 unsigned 状态摘要。

MSI/PKG/DEB 仍必须在各自原生 runner 由安装器工具链生成；本脚本不把交叉编译冒充原生安装验收。
"""
from __future__ import annotations

import argparse
import hashlib
import json
import re
import shutil
import tarfile
import zipfile
from pathlib import Path

SUPPORTED_TARGETS = {
    ("windows", "amd64"),
    ("macos", "amd64"),
    ("linux", "amd64"),
    ("linux", "arm64"),
}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--platform", required=True)
    parser.add_argument("--arch", required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--manifest", type=Path, default=Path("runtime/Cargo.toml"))
    parser.add_argument("--languages-dir", type=Path, default=Path("runtime/languages"))
    parser.add_argument("--commit", default="local")
    parser.add_argument("--build-number", type=int)
    args = parser.parse_args()
    if (args.platform, args.arch) not in SUPPORTED_TARGETS:
        supported = ", ".join(f"{platform}/{arch}" for platform, arch in sorted(SUPPORTED_TARGETS))
        raise SystemExit(
            f"unsupported release target: {args.platform}/{args.arch}; supported: {supported}"
        )
    manifest = args.manifest.read_text(encoding="utf-8")
    version_match = re.search(r"(?ms)^\[workspace\.package\].*?^version\s*=\s*\"([^\"]+)\"", manifest)
    if version_match is None:
        raise SystemExit("workspace Cargo version missing")
    if re.fullmatch(r"\d+\.\d+\.\d+", args.version) is None:
        raise SystemExit(f"invalid release version: {args.version}")
    if args.commit != "local" and re.fullmatch(r"[0-9a-fA-F]{7,64}", args.commit) is None:
        raise SystemExit("commit must be a 7..64 character hexadecimal revision")
    args.out.mkdir(parents=True, exist_ok=True)
    name = f"rivo-{args.version}-{args.platform}-{args.arch}"
    staging = args.out / name
    if staging.exists():
        shutil.rmtree(staging)
    staging.mkdir()
    binary_name = "rivo.exe" if args.platform == "windows" else "rivo"
    shutil.copy2(args.binary, staging / binary_name)
    if args.platform == "windows":
        host = args.binary.with_name("rivo-browser-extension-host.exe")
        if not host.is_file():
            raise SystemExit(f"Windows browser extension host missing: {host}")
        shutil.copy2(host, staging / host.name)
        extension_source = Path("packaging/browser-extension")
        extension_target = staging / "share" / "rivo" / "browser-extension"
        extension_target.mkdir(parents=True)
        for filename in (
            "manifest.json",
            "service-worker.js",
            "content-script.js",
            "install-native-host.ps1",
            "README.md",
        ):
            source = extension_source / filename
            if not source.is_file():
                raise SystemExit(f"Windows browser extension resource missing: {source}")
            shutil.copy2(source, extension_target / filename)
    language_target = staging / "share" / "rivo" / "languages"
    language_target.mkdir(parents=True)
    for locale in ("en-US", "zh-CN"):
        source = args.languages_dir / f"{locale}.ftl"
        if not source.is_file():
            raise SystemExit(f"bundled language file missing: {source}")
        shutil.copy2(source, language_target / source.name)
    shutil.copy2(Path("LICENSE"), staging / "LICENSE")
    shutil.copy2(Path("packaging/archive/README.md"), staging / "README.md")
    examples_target = staging / "share" / "rivo" / "examples"
    examples_target.mkdir(parents=True)
    shutil.copy2(Path("packaging/archive/mock_provider.py"), examples_target / "mock_provider.py")
    brand_source = Path("assets/brand")
    brand_target = staging / "share" / "rivo" / "brand"
    brand_target.mkdir(parents=True)
    for source in sorted(brand_source.iterdir()):
        if source.is_file():
            shutil.copy2(source, brand_target / source.name)
    archive = args.out / (name + (".zip" if args.platform == "windows" else ".tar.gz"))
    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as handle:
            for item in staging.rglob("*"):
                handle.write(item, item.relative_to(args.out))
    else:
        with tarfile.open(archive, "w:gz") as handle:
            handle.add(staging, arcname=staging.name)
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    (args.out / (archive.name + ".sha256")).write_text(f"{digest}  {archive.name}\n", encoding="utf-8")
    metadata = {
        "schema": 2,
        "product": "rivo",
        "version": args.version,
        "platform": args.platform,
        "arch": args.arch,
        "commit": args.commit.lower(),
        "build_number": args.build_number,
        "signature": "unsigned",
        "sha256": digest,
    }
    (args.out / (archive.name + ".json")).write_text(json.dumps(metadata, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
