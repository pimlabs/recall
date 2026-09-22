#!/usr/bin/env python3
"""Rewrite Formula/recall.rb for a release, from that release's checksums.txt.

Usage: update-formula.py <version> <checksums.txt> [<formula path>]

Shared by scripts/release.sh (a release cut from a laptop) and
.github/workflows/release.yml (one cut by CI), so the two cannot drift: the
formula is the one channel whose failure arrives later, on someone else's
machine, as a checksum error that says nothing about why.

The formula installs the prebuilt archives, so it needs the release's own
four checksums rather than a hash of the source tarball. They come from the
checksums.txt the release workflow generated from the artifacts it had just
built — hashing the downloads again would only prove the download worked.
"""

import re
import sys

ARCHIVES = (
    "recall_darwin_arm64.tar.gz",
    "recall_darwin_amd64.tar.gz",
    "recall_linux_arm64.tar.gz",
    "recall_linux_amd64.tar.gz",
)


def rewrite(formula: str, version: str, checksums: str) -> str:
    want = dict.fromkeys(ARCHIVES)
    for line in checksums.splitlines():
        parts = line.split()
        if len(parts) == 2 and parts[1] in want:
            want[parts[1]] = parts[0]

    missing = [name for name, digest in want.items() if not digest]
    if missing:
        sys.exit("checksums.txt has no line for: " + ", ".join(missing))

    formula, n = re.subn(
        r'^  version "[^"]*"$', f'  version "{version}"', formula, count=1, flags=re.M
    )
    if n != 1:
        sys.exit('expected one `  version "..."` line in the formula')

    # Each sha256 belongs to the archive named in the url line above it, so
    # they are rewritten as pairs. Replacing them positionally would silently
    # hand a platform another platform's hash, and brew would report a
    # corrupt download rather than a mistake in this repository.
    for name, digest in want.items():
        pattern = re.compile(
            r'(url "[^"]*/' + re.escape(name) + r'"\n\s*sha256 ")[a-f0-9]*(")'
        )
        formula, n = pattern.subn(lambda m: m.group(1) + digest + m.group(2), formula)
        if n != 1:
            sys.exit(f"expected exactly one url/sha256 pair for {name}, found {n}")
    return formula


def main() -> None:
    if len(sys.argv) not in (3, 4):
        sys.exit(__doc__.split("\n\n")[1])
    version, checksums_path = sys.argv[1], sys.argv[2]
    path = sys.argv[3] if len(sys.argv) == 4 else "Formula/recall.rb"

    with open(checksums_path) as f:
        checksums = f.read()
    with open(path) as f:
        formula = f.read()
    # Computed before the file is opened for writing: `open(..., "w")`
    # truncates, so a refusal inside `rewrite` would otherwise leave an empty
    # formula behind rather than the old one.
    updated = rewrite(formula, version, checksums)
    with open(path, "w") as f:
        f.write(updated)
    print(f"    {len(ARCHIVES)} checksums written to {path}")


if __name__ == "__main__":
    main()
