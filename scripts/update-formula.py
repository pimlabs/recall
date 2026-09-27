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

The archives' names and layout changed after v0.4.5, so this rewrites more
than the hashes: each platform's url names that version's archive, and the
`bin.install` stanza matches what the archive holds. Either way round, so
the formula can be written for any release, older or newer than the one it
was last written for.
"""

import re
import sys

# v0.4.5 is the last release whose archives are named recall_<os>_<arch> and
# hold one binary renamed the same way. Every release after it names them
# recall-<rust target>, holding a recall-<rust target>/ directory with
# `recall` in it. Tags never move, so both stay true for good.
LAST_OLD_STYLE_RELEASE = (0, 4, 5)

# (old name, new name) per platform, in the order the formula lists them.
PLATFORMS = (
    ("recall_darwin_arm64", "recall-aarch64-apple-darwin"),
    ("recall_darwin_amd64", "recall-x86_64-apple-darwin"),
    ("recall_linux_arm64", "recall-aarch64-unknown-linux-gnu"),
    ("recall_linux_amd64", "recall-x86_64-unknown-linux-gnu"),
)

# What `def install` does with a release archive, for each layout. The old
# archive's single file is named for its platform. The new one holds a
# single directory, and Homebrew changes into an archive's only directory
# before `install` runs, so `recall` is right there.
INSTALL_OLD = """\
      # Each archive holds a single file named for its platform —
      # recall_darwin_arm64 and so on. install.sh renames it the same way.
      bin.install Dir["recall_*"].first => "recall"
"""
INSTALL_NEW = """\
      # Each archive holds one directory named for its Rust target —
      # recall-aarch64-apple-darwin/ and so on — with `recall` in it.
      # Homebrew changes into an archive's only directory before this runs.
      bin.install "recall"
"""


def old_style(version: str) -> bool:
    """True for a version released with the old names; a pre-release counts
    as the version it leads up to."""
    core = re.split(r"[-+]", version.lstrip("v"))[0]
    parts = tuple(int(p) for p in core.split("."))
    if len(parts) != 3:
        sys.exit(f"not a release version: {version}")
    return parts <= LAST_OLD_STYLE_RELEASE


def archives(version: str) -> list:
    """This version's four archive file names, in PLATFORMS order."""
    pick = 0 if old_style(version) else 1
    return [names[pick] + ".tar.gz" for names in PLATFORMS]


def rewrite(formula: str, version: str, checksums: str) -> str:
    names = archives(version)
    want = dict.fromkeys(names)
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
    # corrupt download rather than a mistake in this repository. A url line
    # is found by either of its platform's names and left naming this
    # version's.
    for (old, new), name in zip(PLATFORMS, names):
        pattern = re.compile(
            r'(url "[^"]*/)(?:'
            + re.escape(old)
            + "|"
            + re.escape(new)
            + r')\.tar\.gz("\n\s*sha256 ")[a-f0-9]*(")'
        )
        formula, n = pattern.subn(
            lambda m: m.group(1) + name + m.group(2) + want[name] + m.group(3), formula
        )
        if n != 1:
            sys.exit(f"expected exactly one url/sha256 pair for {name}, found {n}")

    stanza = INSTALL_OLD if old_style(version) else INSTALL_NEW
    found = [s for s in (INSTALL_OLD, INSTALL_NEW) if s in formula]
    if len(found) != 1:
        sys.exit("expected the formula's install stanza to be one this script writes")
    return formula.replace(found[0], stanza)


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
    print(f"    {len(PLATFORMS)} checksums written to {path}")


if __name__ == "__main__":
    main()
