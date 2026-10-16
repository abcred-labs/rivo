"""对 scripts/package.py 执行无外部服务依赖的归档回归测试。"""
from __future__ import annotations

import hashlib
import json
import re
import subprocess
import sys
import tarfile
import tempfile
import zipfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "runtime" / "Cargo.toml"


def cargo_version() -> str:
    match = re.search(
        r"(?ms)^\[workspace\.package\].*?^version\s*=\s*\"([^\"]+)\"",
        MANIFEST.read_text(encoding="utf-8"),
    )
    if match is None:
        raise AssertionError("workspace version missing")
    return match.group(1)


def check_manifest(output: Path, version: str, platform: str, arch: str) -> None:
    subprocess.run(
        [
            sys.executable,
            str(ROOT / "scripts" / "release_manifest.py"),
            "--directory",
            str(output),
            "--version",
            version,
            "--platform",
            platform,
            "--arch",
            arch,
            "--commit",
            "0123456789abcdef",
        ],
        cwd=ROOT,
        check=True,
        stdout=subprocess.DEVNULL,
    )
    manifest = json.loads(
        (output / f"release-manifest-{platform}-{arch}.json").read_text(encoding="utf-8")
    )
    archive_name = f"rivo-{version}-{platform}-{arch}.{'zip' if platform == 'windows' else 'tar.gz'}"
    archive = output / archive_name
    assert manifest["schema"] == 2
    assert manifest["product"] == "rivo"
    assert manifest["platform"] == platform
    assert manifest["arch"] == arch
    assert manifest["artifacts"][0]["file"] == archive_name
    assert manifest["artifacts"][0]["sha256"] == hashlib.sha256(archive.read_bytes()).hexdigest()


def main() -> int:
    version = cargo_version()
    with tempfile.TemporaryDirectory(prefix="rivo-package-test-") as directory:
        output = Path(directory)
        binary = output / "rivo.exe"
        binary.write_bytes(b"rivo archive smoke binary\n")
        host = output / "rivo-browser-extension-host.exe"
        host.write_bytes(b"rivo native messaging host smoke binary\n")
        subprocess.run(
            [
                sys.executable,
                str(ROOT / "scripts" / "package.py"),
                "--binary",
                str(binary),
                "--version",
                version,
                "--platform",
                "windows",
                "--arch",
                "amd64",
                "--out",
                str(output),
            ],
            cwd=ROOT,
            check=True,
        )
        archive = output / f"rivo-{version}-windows-amd64.zip"
        checksum = archive.with_name(archive.name + ".sha256")
        manifest = archive.with_name(archive.name + ".json")
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        declared = checksum.read_text(encoding="utf-8").split()[0]
        assert digest == declared, "archive checksum mismatch"
        metadata = json.loads(manifest.read_text(encoding="utf-8"))
        assert metadata["version"] == version
        assert metadata["signature"] == "unsigned"
        assert metadata["sha256"] == digest
        with zipfile.ZipFile(archive) as handle:
            names = set(handle.namelist())
            assert handle.read(f"rivo-{version}-windows-amd64/README.md") == (ROOT / "packaging/archive/README.md").read_bytes()
            assert handle.read(f"rivo-{version}-windows-amd64/share/rivo/examples/mock_provider.py") == (ROOT / "packaging/archive/mock_provider.py").read_bytes()
        assert f"rivo-{version}-windows-amd64/rivo.exe" in names
        assert f"rivo-{version}-windows-amd64/rivo-browser-extension-host.exe" in names
        for filename in ("manifest.json", "service-worker.js", "content-script.js", "install-native-host.ps1", "README.md"):
            assert f"rivo-{version}-windows-amd64/share/rivo/browser-extension/{filename}" in names
        assert f"rivo-{version}-windows-amd64/share/rivo/brand/rivo-mark.svg" in names
        assert f"rivo-{version}-windows-amd64/share/rivo/brand/rivo.ico" in names
        assert f"rivo-{version}-windows-amd64/share/rivo/brand/rivo.icns" in names
        assert f"rivo-{version}-windows-amd64/share/rivo/languages/en-US.ftl" in names
        check_manifest(output, version, "windows", "amd64")
        assert f"rivo-{version}-windows-amd64/share/rivo/languages/zh-CN.ftl" in names

        for platform, arch in (
            ("macos", "amd64"),
            ("macos", "arm64"),
            ("linux", "amd64"),
            ("linux", "arm64"),
        ):
            subprocess.run(
                [
                    sys.executable,
                    str(ROOT / "scripts" / "package.py"),
                    "--binary",
                    str(binary),
                    "--version",
                    version,
                    "--platform",
                    platform,
                    "--arch",
                    arch,
                    "--out",
                    str(output),
                ],
                cwd=ROOT,
                check=True,
            )
            archive = output / f"rivo-{version}-{platform}-{arch}.tar.gz"
            with tarfile.open(archive, "r:gz") as handle:
                names = set(handle.getnames())
            prefix = f"rivo-{version}-{platform}-{arch}"
            assert f"{prefix}/rivo" in names
            assert f"{prefix}/README.md" in names
            assert f"{prefix}/share/rivo/examples/mock_provider.py" in names
            assert not any("browser-extension" in name for name in names)
            assert f"{prefix}/share/rivo/brand/rivo-mark.svg" in names
            assert f"{prefix}/share/rivo/brand/rivo.icns" in names
            assert f"{prefix}/share/rivo/languages/en-US.ftl" in names
            assert f"{prefix}/share/rivo/languages/zh-CN.ftl" in names
            check_manifest(output, version, platform, arch)
        rejected = subprocess.run(
            [
                sys.executable,
                str(ROOT / "scripts" / "package.py"),
                "--binary",
                str(binary),
                "--version",
                version,
                "--platform",
                "solaris",
                "--arch",
                "amd64",
                "--out",
                str(output),
            ],
            cwd=ROOT,
            capture_output=True,
            text=True,
        )
        assert rejected.returncode != 0
        assert "unsupported release target" in rejected.stderr
        host.unlink()
        missing_host = subprocess.run(
            [sys.executable, str(ROOT / "scripts" / "package.py"), "--binary", str(binary),
             "--version", version, "--platform", "windows", "--arch", "amd64", "--out", str(output)],
            cwd=ROOT, capture_output=True, text=True,
        )
        assert missing_host.returncode != 0
        assert "Windows browser extension host missing" in missing_host.stderr
    print("archive layout check passed; native installer layout is checked separately")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
