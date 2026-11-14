#!/usr/bin/env python3
"""Every Rust toolchain CI installs is the version deploy/Dockerfile pins.

The toolchain is pinned rather than left at `stable`, so a new Rust release
cannot turn CI red, or change what a release is built with, until the pin
moves. It is written in more than one place: `ARG RUST_VERSION` in
deploy/Dockerfile, and the `toolchain:` of every dtolnay/rust-toolchain step
under .github/. This fails when a step names another version or none, so a
bump that missed a place says which.

The checker is also run against steps it must refuse, so a pattern that
quietly accepts everything fails here rather than passing forever.
"""

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
DOCKERFILE_PIN = re.compile(r"^ARG RUST_VERSION=(\S+)$", re.MULTILINE)
STEP = re.compile(r"^(\s*)-\s+uses:\s*dtolnay/rust-toolchain@")
TOOLCHAIN = re.compile(r"^\s+toolchain:\s*(\S+)\s*$")


def problems(text, version):
    """(line number, why) for each dtolnay/rust-toolchain step in `text`
    that does not install `version`."""
    lines = text.splitlines()
    found = []
    for start, line in enumerate(lines):
        step = STEP.match(line)
        if not step:
            continue
        dash = len(step.group(1))
        named = None
        for inner in lines[start + 1 :]:
            if inner.strip() and len(inner) - len(inner.lstrip()) <= dash:
                break
            match = TOOLCHAIN.match(inner)
            if match:
                named = match.group(1)
        if named is None:
            found.append((start + 1, "names no toolchain, so it installs whatever is stable"))
        elif named != version:
            found.append((start + 1, f"installs {named}, not {version}"))
    return found


def check_the_checker():
    uses = "- uses: dtolnay/rust-toolchain@6bed0761d98439e5a578e2877258200ad565ba87 # stable branch"
    must_refuse = [
        f"  {uses}\n",
        f"  {uses}\n    with:\n      targets: x\n",
        f"  {uses}\n    with:\n      toolchain: 1.99.1\n",
        f"  {uses}\n  - run: true\n    with:\n      toolchain: 1.100.0\n",
    ]
    must_accept = [
        f"  {uses}\n    with:\n      toolchain: 1.100.0\n      targets: x\n",
        f"  {uses}\n    with:\n      # a comment\n\n      toolchain: 1.100.0\n",
    ]
    wrong = [f"accepted {s!r}" for s in must_refuse if not problems(s, "1.100.0")]
    wrong += [f"refused {s!r}" for s in must_accept if problems(s, "1.100.0")]
    return wrong


def main():
    wrong = check_the_checker()
    if wrong:
        print("toolchain-pin-check: the checker itself is broken:", *wrong, sep="\n  ")
        return 1
    pin = DOCKERFILE_PIN.search((ROOT / "deploy" / "Dockerfile").read_text())
    if not pin:
        print("toolchain-pin-check: deploy/Dockerfile has no `ARG RUST_VERSION=`")
        return 1
    version = pin.group(1)
    files = sorted((ROOT / ".github").rglob("*.yml")) + sorted((ROOT / ".github").rglob("*.yaml"))
    steps = 0
    failures = []
    for path in files:
        text = path.read_text()
        steps += sum(1 for line in text.splitlines() if STEP.match(line))
        failures += [f"{path.relative_to(ROOT)}:{n}: {why}" for n, why in problems(text, version)]
    if steps == 0:
        print("toolchain-pin-check: found no dtolnay/rust-toolchain step; the pattern is wrong")
        return 1
    if failures:
        print(f"toolchain-pin-check: give these `toolchain: {version}`, the version deploy/Dockerfile pins:")
        print(*failures, sep="\n")
        return 1
    print(f"toolchain-pin-check: {steps} dtolnay/rust-toolchain steps, every one on {version}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
