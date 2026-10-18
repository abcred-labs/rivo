#!/usr/bin/env python3
"""Validate public documentation language pairs and release-document hygiene."""
from __future__ import annotations

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PUBLIC_ROOTS = [ROOT / "docs", ROOT / "packaging", ROOT / "runtime"]
SPECIAL_PAIRS = {
    ROOT / "SOURCE_CONFIGURATION_AND_TESTING.md": ROOT / "SOURCE_CONFIGURATION_AND_TESTING.en.md",
}
PLACEHOLDER_RE = re.compile(
    r"(?i)(?<![\w-])(?:todo|tbd|coming\s+soon|placeholder|占位)(?![\w-])"
)
ENGLISH_LINK_RE = re.compile(r"\[English\]\(([^)]+)\)")
CHINESE_LINK_RE = re.compile(
    r"\[(?:简体中文|中文|Chinese(?:\s+version)?)[^\]]*\]\(([^)]+)\)", re.IGNORECASE
)

CRITICAL_REQUIREMENTS: dict[str, tuple[tuple[str, ...], ...]] = {
    "docs/guides/one-click-install.md": ((".zip", ".tar.gz"), ("sha256", "SHA-256"), ("upgrade", "升级")),
    "docs/guides/tui.md": (("rivo tui",), ("rivo run",), ("/settings", "F6"), ("confirmation", "确认", "policy")),
    "docs/guides/configuration/vendor-management.md": (("rivo config vendor",), ("rivo config model set",), ("custom", "自定义")),
    "docs/guides/configuration/model-management.md": (("rivo config model",), ("supported_protocols", "protocol"), ("canonical", "精确")),
    "docs/guides/configuration/provider-management.md": (("rivo config provider",), ("base_url", "base-url"), ("refresh", "刷新")),
    "docs/guides/configuration/credential-management.md": (("rivo config credentials",), ("api-key-env", "from-env"), ("secret", "密钥", "credential")),
    "docs/guides/configuration/provider-model-refresh.md": (("provider models refresh",), ("discovery",), ("404", "401", "失败")),
    "docs/guides/configuration/model-routing.md": (("config route",), ("auto",), ("provider", "路由")),
    "docs/guides/configuration/troubleshooting.md": (("config validate",), ("refresh", "刷新"), ("error", "failure", "失败", "排障")),
}


def public_pages() -> list[Path]:
    pages: set[Path] = set(ROOT.glob("README*.md"))
    pages.update(SPECIAL_PAIRS)
    pages.update(SPECIAL_PAIRS.values())
    for base in PUBLIC_ROOTS:
        if base == ROOT:
            pages.update(ROOT.glob("README*.md"))
        elif base == ROOT / "runtime":
            pages.update(base.glob("README*.md"))
        else:
            pages.update(base.rglob("*.md"))
    return sorted(p for p in pages if ".git" not in p.parts)


def canonical_for(page: Path) -> Path:
    if page.name.endswith(".zh-CN.md"):
        return page.with_name(page.name[: -len(".zh-CN.md")] + ".md")
    return page


def is_chinese_page(page: Path) -> bool:
    return page.name.endswith(".zh-CN.md") or page in SPECIAL_PAIRS


def language_switch_errors(page: Path, text: str) -> list[str]:
    chinese_page = is_chinese_page(page)
    pattern = ENGLISH_LINK_RE if chinese_page else CHINESE_LINK_RE
    language_name = "English" if chinese_page else "Chinese"
    links = pattern.findall(text)
    if not links:
        return [f"missing {language_name} language link: {page.relative_to(ROOT)}"]
    errors: list[str] = []
    for target in links:
        if target.startswith(("http://", "https://", "#")):
            continue
        resolved = (page.parent / target).resolve()
        if not resolved.is_file():
            errors.append(f"{language_name} link does not exist: {page.relative_to(ROOT)} -> {target}")
        elif chinese_page and is_chinese_page(resolved):
            errors.append(f"English link points to Chinese page: {page.relative_to(ROOT)} -> {target}")
        elif not chinese_page and not is_chinese_page(resolved):
            errors.append(f"Chinese link points to English page: {page.relative_to(ROOT)} -> {target}")
    return errors


def check() -> list[str]:
    failures: list[str] = []
    pages = public_pages()
    for page in pages:
        text = page.read_text(encoding="utf-8")
        canonical = canonical_for(page)
        if page.name.endswith(".zh-CN.md"):
            if not canonical.is_file():
                failures.append(f"orphan Chinese page: {page.relative_to(ROOT)}")
            elif not re.search(r"[\u4e00-\u9fff]", text):
                failures.append(f"Chinese page has no Chinese content: {page.relative_to(ROOT)}")
        elif page.name.endswith(".md"):
            if page in SPECIAL_PAIRS:
                chinese = page
            else:
                chinese = page.with_name(page.stem + ".zh-CN.md")
            if page not in SPECIAL_PAIRS and page not in SPECIAL_PAIRS.values() and not chinese.is_file():
                failures.append(f"missing Chinese page: {page.relative_to(ROOT)}")
            elif not re.search(r"[A-Za-z]", text) and page not in SPECIAL_PAIRS and page not in SPECIAL_PAIRS.values():
                failures.append(f"English page has no English content: {page.relative_to(ROOT)}")
        failures.extend(language_switch_errors(page, text))
        for match in PLACEHOLDER_RE.finditer(text):
            failures.append(f"unfinished placeholder word at {page.relative_to(ROOT)}:{text.count(chr(10), 0, match.start()) + 1}")

    for chinese, english in SPECIAL_PAIRS.items():
        if not chinese.is_file() or not english.is_file():
            failures.append(f"missing special language pair: {chinese.relative_to(ROOT)} / {english.relative_to(ROOT)}")
        else:
            if not re.search(r"[一-鿿]", chinese.read_text(encoding="utf-8")):
                failures.append(f"special Chinese page has no Chinese content: {chinese.relative_to(ROOT)}")
            if not re.search(r"[A-Za-z]", english.read_text(encoding="utf-8")):
                failures.append(f"special English page has no English content: {english.relative_to(ROOT)}")

    for relative, groups in CRITICAL_REQUIREMENTS.items():
        page = ROOT / relative
        chinese = page.with_name(page.stem + ".zh-CN.md")
        for candidate in (page, chinese):
            text = candidate.read_text(encoding="utf-8").lower()
            for alternatives in groups:
                if not any(option.lower() in text for option in alternatives):
                    failures.append(
                        f"critical page missing {alternatives}: {candidate.relative_to(ROOT)}"
                    )
    return failures


def main() -> int:
    failures = check()
    if failures:
        print("\n".join(failures))
        return 1
    print(f"checked {len(public_pages())} public Markdown files; language pairs and release-doc hygiene passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
