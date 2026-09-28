#!/usr/bin/env python3
"""Every action a workflow uses is pinned to a full commit SHA.

A tag such as `@v5` is a pointer its owner can move, and a moved tag runs
new code with this repository's secrets: the deploy job's SSH key, the
release's publishing tokens. A 40-character commit SHA cannot move. So every
`uses:` under .github/ names one, followed by the version it is as a comment
(`@<sha> # v5.1.0`), which is what .github/dependabot.yml reads to propose
updates. Local actions and workflows (`./...`) are this repository's own
files and are exempt.

The checker is also run against lines it must refuse, so a regex that
quietly accepts everything fails here rather than passing forever.
"""

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
USES = re.compile(r"^\s*(?:-\s+)?uses:\s*(.*?)\s*$")
PINNED = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_./-]+@[0-9a-f]{40} # \S.*$")


def problem(ref):
    """Why a `uses:` value is not acceptable, or None when it is."""
    if ref.startswith("./"):
        return None
    if PINNED.match(ref):
        return None
    if re.search(r"@[0-9a-f]{40}$", ref):
        return "pinned, but with no '# <version>' comment for Dependabot to read"
    return "not pinned to a full commit SHA"


def check_files():
    failures = []
    files = sorted((ROOT / ".github").rglob("*.yml")) + sorted((ROOT / ".github").rglob("*.yaml"))
    count = 0
    for path in files:
        for number, line in enumerate(path.read_text().splitlines(), 1):
            match = USES.match(line)
            if not match:
                continue
            count += 1
            why = problem(match.group(1))
            if why:
                failures.append(f"{path.relative_to(ROOT)}:{number}: {match.group(1)}: {why}")
    return count, failures


def check_the_checker():
    sha = "fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09"
    must_refuse = [
        "actions/checkout@v5",
        "actions/checkout@main",
        f"actions/checkout@{sha}",
        f"actions/checkout@{sha[:12]} # v5.1.0",
        f"actions/checkout@{sha.upper()} # v5.1.0",
        "docker://alpine:3",
    ]
    must_accept = [
        f"actions/checkout@{sha} # v5.1.0",
        f"github/codeql-action/init@{sha} # v3.29.0",
        "./.github/actions/rust-setup",
        "./.github/workflows/deploy.yml",
    ]
    wrong = [f"accepted {r!r}" for r in must_refuse if problem(r) is None]
    wrong += [f"refused {r!r}" for r in must_accept if problem(r) is not None]
    return wrong


def main():
    wrong = check_the_checker()
    if wrong:
        print("actions-pin-check: the checker itself is broken:", *wrong, sep="\n  ")
        return 1
    count, failures = check_files()
    if count == 0:
        print("actions-pin-check: found no `uses:` lines at all; the pattern is wrong")
        return 1
    if failures:
        print("actions-pin-check: pin these to a full commit SHA with '# <version>' after it:")
        print(*failures, sep="\n")
        return 1
    print(f"actions-pin-check: {count} uses: lines, every one pinned or local")
    return 0


if __name__ == "__main__":
    sys.exit(main())
