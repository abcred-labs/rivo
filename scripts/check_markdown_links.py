from __future__ import annotations

import re
import sys
import urllib.parse
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
EXCLUDED_PARTS = {".git", "target", "artifacts", "node_modules", "bin", "obj", ".codex-dev-tmp-tasks"}
LINK_RE = re.compile(r"(?<!!)\[[^\]]*\]\(([^)]+)\)")
HEADING_RE = re.compile(r"^#{1,6}\s+(.+?)\s*$")


def slug(text: str) -> str:
    text = re.sub(r"[`*_~]", "", text.strip().lower())
    text = re.sub(r"[^\w\-\u4e00-\u9fff ]", "", text)
    return re.sub(r"[\s_]+", "-", text).strip("-")


def anchors(path: Path) -> set[str]:
    result: set[str] = set()
    counts: dict[str, int] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        match = HEADING_RE.match(line)
        if not match:
            continue
        base = slug(match.group(1))
        count = counts.get(base, 0)
        counts[base] = count + 1
        result.add(base if count == 0 else f"{base}-{count}")
    return result


def main() -> int:
    failures: list[str] = []
    markdown_files = [
        path
        for path in ROOT.rglob("*.md")
        if not EXCLUDED_PARTS.intersection(path.relative_to(ROOT).parts)
    ]
    for source in markdown_files:
        content = source.read_text(encoding="utf-8")
        for match in LINK_RE.finditer(content):
            raw_target = match.group(1).strip().strip("<>")
            if raw_target.startswith(("http://", "https://", "mailto:")):
                continue
            path_part, _, anchor = raw_target.partition("#")
            target = source if not path_part else source.parent / urllib.parse.unquote(path_part)
            target = target.resolve()
            line = content.count("\n", 0, match.start()) + 1
            if not target.exists():
                failures.append(f"{source.relative_to(ROOT)}:{line}: missing {raw_target}")
                continue
            if anchor and target.suffix.lower() == ".md" and anchor not in anchors(target):
                failures.append(
                    f"{source.relative_to(ROOT)}:{line}: missing anchor #{anchor} in "
                    f"{target.relative_to(ROOT)}"
                )
    if failures:
        print("\n".join(failures), file=sys.stderr)
        return 1
    print(f"checked {len(markdown_files)} Markdown files")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
