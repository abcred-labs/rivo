"""Static verifier for the Chrome/Edge Manifest V3 browser extension package."""

from __future__ import annotations

import json
import base64
import hashlib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1] / "packaging" / "browser-extension"


def load(path: Path) -> dict:
    with path.open(encoding="utf-8") as stream:
        value = json.load(stream)
    if not isinstance(value, dict):
        raise SystemExit(f"{path}: root must be an object")
    return value


def main() -> None:
    manifest = load(ROOT / "manifest.json")
    if manifest.get("manifest_version") != 3:
        raise SystemExit("manifest must use Manifest V3")
    key = manifest.get("key")
    if not isinstance(key, str) or not key:
        raise SystemExit("manifest must pin a developer public key")
    digest = hashlib.sha256(base64.b64decode(key)).digest()[:16]
    extension_id = "".join("abcdefghijklmnop"[byte >> shift & 0x0F] for byte in digest for shift in (4, 0))
    if extension_id != "adnanfbbgelpelombhdjflmcggepbcee":
        raise SystemExit(f"manifest developer key has unexpected extension ID: {extension_id}")
    permissions = set(manifest.get("permissions", []))
    required = {"nativeMessaging", "tabs", "storage"}
    if not required <= permissions:
        raise SystemExit(f"manifest missing permissions: {sorted(required - permissions)}")
    if permissions != required:
        raise SystemExit(f"manifest has permissions outside the reviewed minimum: {sorted(permissions - required)}")
    if manifest.get("host_permissions") != ["<all_urls>"]:
        raise SystemExit("manifest host permissions must be explicit <all_urls>")
    if manifest.get("background", {}).get("service_worker") != "service-worker.js":
        raise SystemExit("service worker entry is missing")
    for filename in (
        "service-worker.js",
        "content-script.js",
        "install-native-host.ps1",
    ):
        if not (ROOT / filename).is_file():
            raise SystemExit(f"missing package file: {filename}")
    smoke = ROOT.parent.parent / "scripts" / "browser_extension_smoke.ps1"
    if not smoke.is_file():
        raise SystemExit(f"missing independent smoke script: {smoke}")
    multi_smoke = ROOT.parent.parent / "scripts" / "browser_extension_host_multi_smoke.ps1"
    if not multi_smoke.is_file():
        raise SystemExit(f"missing multi-instance host smoke script: {multi_smoke}")
    profile_smoke = ROOT.parent.parent / "scripts" / "browser_extension_profiles_smoke.ps1"
    if not profile_smoke.is_file():
        raise SystemExit(f"missing real-profile recovery smoke script: {profile_smoke}")
    fixture = ROOT.parent.parent / "scripts" / "fixtures" / "browser-extension-smoke.html"
    if not fixture.is_file():
        raise SystemExit(f"missing real-page smoke fixture: {fixture}")
    for script in (smoke, profile_smoke):
        smoke_text = script.read_text(encoding="utf-8")
        if any(token in smoke_text for token in ("DevTools", "/json/", "CDP", "remote-debugging")):
            raise SystemExit(f"independent smoke must not use CDP or DevTools discovery: {script}")
    for browser in ("chrome", "edge"):
        host = load(ROOT / "native-host" / f"com.rivo.browser.{browser}.json")
        if host.get("name") != "com.rivo.browser":
            raise SystemExit(f"{browser} host name mismatch")
        if not host.get("path", "").endswith("rivo-browser-extension-host.exe"):
            raise SystemExit(f"{browser} host must use standalone executable")
        origins = host.get("allowed_origins", [])
        if origins != ["chrome-extension://__EXTENSION_ID__/"]:
            raise SystemExit(f"{browser} host origin must be restricted to the fixed Rivo extension placeholder")
    print("browser extension package: OK")


if __name__ == "__main__":
    main()
