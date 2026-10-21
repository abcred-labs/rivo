#!/usr/bin/env python3
"""Check the GitHub repository security settings required by docs/runbooks/releasing.md.

Needs a token that can read repository administration data:
    GH_TOKEN=<token> python scripts/check_repo_security.py [--repo owner/name]
Without GH_TOKEN only the publicly visible settings are checked and the rest is reported
as UNKNOWN. Exit status is 1 if any setting is verifiably missing, 0 otherwise.
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.error
import urllib.request


def call(repo: str, path: str, token: str | None):
    request = urllib.request.Request(
        f"https://api.github.com/repos/{repo}{path}",
        headers={"Accept": "application/vnd.github+json", "User-Agent": "rivo-security-check"},
    )
    if token:
        request.add_header("Authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            body = response.read().decode("utf-8")
            return response.status, json.loads(body) if body else None
    except urllib.error.HTTPError as error:
        return error.code, None


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo", default="abcred-labs/rivo")
    args = parser.parse_args()
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    failures = 0

    def report(name: str, ok: bool | None, detail: str = "") -> None:
        nonlocal failures
        status = "UNKNOWN" if ok is None else "OK" if ok else "MISSING"
        if ok is False:
            failures += 1
        print(f"[{status:7}] {name}{': ' + detail if detail else ''}")

    _, repo = call(args.repo, "", token)
    report("repository is public and readable", bool(repo), "" if repo else "cannot read the repository")
    default_branch = (repo or {}).get("default_branch", "main")

    code, rulesets = call(args.repo, "/rulesets", token)
    rulesets = rulesets or []
    tag_rule = [r for r in rulesets if r.get("target") == "tag" and r.get("enforcement") == "active"]
    report("active tag ruleset (immutable release tags)", bool(tag_rule), ", ".join(r["name"] for r in tag_rule))
    branch_rule = [r for r in rulesets if r.get("target") == "branch" and r.get("enforcement") == "active"]
    code_bp, _ = call(args.repo, f"/branches/{default_branch}/protection", token)
    if branch_rule or code_bp == 200:
        report(f"{default_branch} branch protection or ruleset", True)
    elif code_bp in (401, 403):
        report(f"{default_branch} branch protection or ruleset", None, "token lacks administration read access")
    else:
        report(f"{default_branch} branch protection or ruleset", False, "no active branch ruleset or protection")

    analysis = (repo or {}).get("security_and_analysis")
    if analysis is None:
        for name in ("secret scanning", "secret scanning push protection", "Dependabot security updates"):
            report(name, None, "needs GH_TOKEN with repository administration access")
    else:
        for key, name in (
            ("secret_scanning", "secret scanning"),
            ("secret_scanning_push_protection", "secret scanning push protection"),
            ("dependabot_security_updates", "Dependabot security updates"),
        ):
            report(name, (analysis.get(key) or {}).get("status") == "enabled")

    code, pvr = call(args.repo, "/private-vulnerability-reporting", token)
    if code == 200 and pvr is not None:
        report("private vulnerability reporting", bool(pvr.get("enabled")))
    else:
        report("private vulnerability reporting", None, f"HTTP {code}")

    code, _ = call(args.repo, "/contents/.github/dependabot.yml", token)
    report("Dependabot configuration file", code == 200)
    code, _ = call(args.repo, "/contents/SECURITY.md", token)
    report("SECURITY.md", code == 200)

    print("\nFailures:", failures)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
