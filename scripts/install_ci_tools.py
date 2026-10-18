"""在 Linux amd64 CI 中安装经过固定 SHA-256 校验的非 Cargo 工具。"""
from __future__ import annotations

import hashlib
import io
import os
import platform
import tarfile
import urllib.request
from pathlib import Path

TOOLS = {
    "gitleaks": (
        "https://github.com/gitleaks/gitleaks/releases/download/v8.30.1/gitleaks_8.30.1_linux_x64.tar.gz",
        "551f6fc83ea457d62a0d98237cbad105af8d557003051f41f3e7ca7b3f2470eb",
    ),
    "actionlint": (
        "https://github.com/rhysd/actionlint/releases/download/v1.7.12/actionlint_1.7.12_linux_amd64.tar.gz",
        "8aca8db96f1b94770f1b0d72b6dddcb1ebb8123cb3712530b08cc387b349a3d8",
    ),
}


def main() -> None:
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        raise SystemExit("该安装器仅用于 Linux amd64 CI")
    directory = Path(os.environ["RUNNER_TEMP"]) / "rivo-tools"
    directory.mkdir(exist_ok=True)
    for name, (url, expected) in TOOLS.items():
        with urllib.request.urlopen(url, timeout=60) as response:
            archive = response.read()
        if hashlib.sha256(archive).hexdigest() != expected:
            raise SystemExit(f"下载摘要不匹配：{name}")
        with tarfile.open(fileobj=io.BytesIO(archive)) as handle:
            executable = handle.extractfile(name).read()
        target = directory / name
        target.write_bytes(executable)
        target.chmod(0o755)
        print(f"verified {name}")
    with Path(os.environ["GITHUB_PATH"]).open("a", encoding="utf-8") as handle:
        handle.write(str(directory) + "\n")


if __name__ == "__main__":
    main()
