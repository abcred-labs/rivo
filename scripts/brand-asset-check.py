"""Validate the checked-in Rivo brand assets used by native packages."""
from __future__ import annotations

import re
import struct
import xml.etree.ElementTree as ET
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
BRAND = ROOT / "assets" / "brand"
ALLOWED_COLORS = {"#F26A3D", "#1F2328", "#FFFFFF"}
PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"


def png_size(path: Path) -> tuple[int, int]:
    data = path.read_bytes()
    if data[:8] != PNG_SIGNATURE or data[12:16] != b"IHDR":
        raise AssertionError(f"invalid PNG: {path}")
    return struct.unpack(">II", data[16:24])


def check_svgs() -> None:
    for path in BRAND.glob("*.svg"):
        root = ET.fromstring(path.read_bytes())
        if root.tag.rsplit("}", 1)[-1] != "svg":
            raise AssertionError(f"SVG root is invalid: {path}")
        source = path.read_text(encoding="utf-8")
        if re.search(r"<(?:text|rect|linearGradient|radialGradient)\b|font-family", source):
            raise AssertionError(f"SVG contains unsupported branding primitive: {path}")
        colors = set(re.findall(r"#[0-9A-Fa-f]{6}", source))
        if not colors <= ALLOWED_COLORS:
            raise AssertionError(f"unexpected SVG colors in {path}: {sorted(colors)}")


def check_ico() -> None:
    data = (BRAND / "rivo.ico").read_bytes()
    reserved, kind, count = struct.unpack("<HHH", data[:6])
    if (reserved, kind, count) != (0, 1, 7):
        raise AssertionError("rivo.ico must contain seven icon images")
    sizes = set()
    for index in range(count):
        offset = 6 + index * 16
        width, height = data[offset], data[offset + 1]
        sizes.add((width or 256, height or 256))
    expected = {(size, size) for size in (16, 24, 32, 48, 64, 128, 256)}
    if sizes != expected:
        raise AssertionError(f"unexpected ICO sizes: {sorted(sizes)}")


def check_icns() -> None:
    data = (BRAND / "rivo.icns").read_bytes()
    if data[:4] != b"icns" or struct.unpack(">I", data[4:8])[0] != len(data):
        raise AssertionError("invalid ICNS header or length")
    expected = {b"icp4", b"icp5", b"icp6", b"ic07", b"ic08", b"ic09"}
    found = set()
    offset = 8
    while offset < len(data):
        chunk_type = data[offset : offset + 4]
        chunk_length = struct.unpack(">I", data[offset + 4 : offset + 8])[0]
        if chunk_length < 8 or offset + chunk_length > len(data):
            raise AssertionError("invalid ICNS chunk")
        if data[offset + 8 : offset + 16] != PNG_SIGNATURE:
            raise AssertionError("ICNS chunks must contain PNG data")
        found.add(chunk_type)
        offset += chunk_length
    if found != expected:
        raise AssertionError(f"unexpected ICNS types: {sorted(found)}")


def main() -> int:
    check_svgs()
    if png_size(BRAND / "rivo-mark.png") != (512, 512):
        raise AssertionError("rivo-mark.png must be 512x512")
    if png_size(BRAND / "rivo-logo.png") != (1200, 404):
        raise AssertionError("rivo-logo.png must be 1200x404")
    check_ico()
    check_icns()
    print("brand asset check passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
