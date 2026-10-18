"""解压并运行当前平台的真实发布归档；所有配置、数据和 HTTP fixture 均隔离。"""
from __future__ import annotations

import argparse
import functools
import hashlib
import importlib.util
import json
import os
import re
import shutil
import subprocess
import tarfile
import tempfile
import threading
import urllib.request
import zipfile
from contextlib import contextmanager
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


@contextmanager
def serving(handler):
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}"
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


class QuietFiles(SimpleHTTPRequestHandler):
    def log_message(self, *_):
        pass


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--platform", choices=("windows", "linux", "macos"), required=True)
    parser.add_argument("--arch", choices=("amd64", "arm64"), required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    directory = args.directory.resolve()
    manifest_file = directory / f"release-manifest-{args.platform}-{args.arch}.json"
    manifest = json.loads(manifest_file.read_text(encoding="utf-8"))
    assert manifest["version"] == args.version
    assert (manifest["platform"], manifest["arch"]) == (args.platform, args.arch)
    archive = directory / manifest["artifacts"][0]["file"]
    assert archive.parent == directory and archive.is_file()
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    assert digest == manifest["artifacts"][0]["sha256"]
    assert archive.stat().st_size == manifest["artifacts"][0]["bytes"]
    assert archive.with_name(archive.name + ".sha256").read_text().split() == [digest, archive.name]
    checks = ["archive checksum, size and platform manifest"]

    with tempfile.TemporaryDirectory(prefix="rivo-release-smoke-") as temporary:
        isolated = Path(temporary)
        unpacked = isolated / "unpacked"
        if archive.suffix == ".zip":
            with zipfile.ZipFile(archive) as handle:
                handle.extractall(unpacked)
        else:
            with tarfile.open(archive, "r:gz") as handle:
                handle.extractall(unpacked, filter="data")
        package = unpacked / f"rivo-{args.version}-{args.platform}-{args.arch}"
        binary = package / ("rivo.exe" if args.platform == "windows" else "rivo")
        workspace = isolated / "workspace"
        workspace.mkdir()
        config = isolated / "config/config.toml"
        config.parent.mkdir()
        config.write_text("version = 6\n", encoding="utf-8")
        environment = {k: v for k, v in os.environ.items() if not k.upper().startswith("RIVO_")}
        environment.update(RIVO_CONFIG=str(config), RIVO_DATA_DIR=str(isolated / "data"),
                           RIVO_LANGUAGE_DIR=str(isolated / "languages"), NO_PROXY="127.0.0.1,localhost")

        def run(*command: str, extra_env=None) -> str:
            result = subprocess.run([str(binary), *command], cwd=workspace,
                                    env=environment | (extra_env or {}), input="",
                                    capture_output=True, encoding="utf-8", timeout=90)
            if result.returncode:
                raise RuntimeError(f"{command}: exit {result.returncode}\n{result.stdout}\n{result.stderr}")
            return result.stdout

        version = run("--version").strip()
        assert re.search(rf"\b{re.escape(args.version)}\b", version), version
        assert f"({manifest['commit']})" in version, "binary commit differs from release manifest"
        assert "Usage" in run("--help")
        assert run("--language", "zh-CN", "--help")
        run("config", "validate")
        checks.append("extracted binary version, English/Chinese help and fresh schema 6 config")
        for locale in ("en-US", "zh-CN"):
            language = package / f"share/rivo/languages/{locale}.ftl"
            run("language", "validate", str(language))
        checks.append("packaged en-US/zh-CN FTL validation")
        notices = package / "share/rivo/licenses"
        for name in ("dependencies.json", "THIRD_PARTY_LICENSES.txt", "LICENSE-BIP39.txt"):
            assert (notices / name).read_bytes() == (ROOT / "licenses" / name).read_bytes()
        checks.append("complete dependency licenses preserved")

        fixture_file = package / "share/rivo/examples/mock_provider.py"
        spec = importlib.util.spec_from_file_location("release_mock", fixture_file)
        fixture = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(fixture)
        with serving(fixture.Fixture) as base:
            run("config", "vendor", "set", "mock-local", "--display-name", "Local Mock")
            run("config", "model", "set", "mock-local-model", "--vendor", "mock-local",
                "--display-name", "Local Mock", "--protocols", "openai_chat_completions")
            run("config", "provider", "set", "mock-local", "--base-url", base + "/v1")
            run("config", "provider", "models", "refresh", "mock-local")
            run("config", "route", "--vendor", "mock-local", "--model", "mock-local-model",
                "--provider-id", "mock-local")
            response = run("run", "--vendor", "mock-local", "--model", "mock-local-model",
                           "--provider-id", "mock-local", "--workspace", str(workspace),
                           "--prompt", "Reply RIVO_MOCK_OK")
            assert "RIVO_MOCK_OK" in response, response
        checks.append("packaged mock provider discovery, routing and streamed task")

        mirror = isolated / "mirror"
        platform_dir = mirror / f"{args.platform}-{args.arch}"
        platform_dir.mkdir(parents=True)
        shutil.copy2(manifest_file, platform_dir / "release-manifest.json")
        shutil.copy2(archive, platform_dir / archive.name)
        handler = functools.partial(QuietFiles, directory=str(mirror))
        with serving(handler) as base:
            output = run("upgrade", extra_env={"RIVO_RELEASE_BASE_URL": base})
            assert args.version in output
            # 验证实际 HTTP 资产与 manifest 的摘要，不声称 upgrade 自动安装。
            with urllib.request.urlopen(f"{base}/{platform_dir.name}/{archive.name}", timeout=30) as response:
                downloaded = hashlib.file_digest(response, "sha256").hexdigest()
            assert downloaded == digest
        checks.append("upgrade reads served manifest; HTTP archive download matches checksum")

        if args.platform == "windows":
            subprocess.run(
                ["pwsh", "-NoProfile", "-File",
                 str(ROOT / "scripts/browser_extension_host_multi_smoke.ps1"),
                 "-HostExecutable", str(package / "rivo-browser-extension-host.exe")],
                cwd=workspace, env=environment, check=True, timeout=90,
            )
            checks.append("extracted Windows native host multi-instance request/response and EOF recovery")

    report = {"schema": 1, "version": args.version, "platform": args.platform, "arch": args.arch,
              "commit": manifest["commit"], "archive_sha256": digest, "status": "completed", "checks": checks}
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(f"release archive smoke: OK ({args.platform}/{args.arch})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
