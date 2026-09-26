#!/usr/bin/env python3
"""Generate the three winget-pkgs manifest files for a Recall release.

Usage: generate_manifest.py <version> <checksums.txt> [<output dir>]

winget-pkgs wants a package's *first* version submitted by hand — the
`winget` job in .github/workflows/release.yml (vedantmgoyal9/winget-releaser)
only updates a package that already exists there, it does not create one.
This script exists for that one-time submission, and for anyone who wants to
inspect or hand-edit what a release's manifest will look like without
re-deriving the shape from scratch. It is not run by CI or by the release
workflow; komac/wingetcreate write their own manifest during the automated
update.

It writes, under <output dir> (default: packaging/winget/out/<version>),
the same path winget-pkgs itself uses — manifests/<first letter>/<publisher
segment>/<package>/<version>/ — so the output directory's contents can be
copied straight into a winget-pkgs checkout:

  manifests/p/PimLabs/Recall/<version>/PimLabs.Recall.yaml
  manifests/p/PimLabs/Recall/<version>/PimLabs.Recall.installer.yaml
  manifests/p/PimLabs/Recall/<version>/PimLabs.Recall.locale.en-US.yaml

The installer manifest needs both Windows archives' SHA-256, which come from
the release's own checksums.txt — the same file scripts/update-formula.py
reads for Homebrew, for the same reason: hand-typing a hash risks a typo
nothing catches, and hashing the downloads again would only prove the
download worked, not that it matches what shipped.

Schema version and shape: see packaging/winget/schemas/SOURCE.txt for where
the vendored JSON schemas came from, and packaging/winget/test_manifest.py
for the check that this script's output still satisfies them.
"""

import re
import sys
from pathlib import Path

IDENTIFIER = "PimLabs.Recall"
PUBLISHER = "pimlabs"
PACKAGE_NAME = "Recall"
LICENSE = "MIT"
MONIKER = "recall"
DEFAULT_LOCALE = "en-US"
SCHEMA_VERSION = "1.28.0"  # kept in step with packaging/winget/schemas/SOURCE.txt

REPO = "https://github.com/pimlabs/recall"
SHORT_DESCRIPTION = "Sync Claude Code's auto memory across machines and cloud sessions"
TAGS = ["claude", "claude-code", "cli", "memory", "sync"]

# One installer per architecture, each a zip — the same archives
# .github/workflows/release.yml's `build` job publishes (its "Package
# (Windows)" step) and docs/reference/releasing.md documents. Keep this in
# step with both if either ever changes.
#
# v0.4.5 is the last release whose zips are named recall_windows_<arch>.zip
# and hold a bare recall.exe. Every release after it names them
# recall-<rust target>.zip, holding a recall-<rust target>\ directory with
# recall.exe in it. Tags never move, so both stay true for good.
LAST_OLD_STYLE_RELEASE = (0, 4, 5)

# winget architecture -> (old archive stem, Rust target).
PLATFORMS = {
    "x64": ("recall_windows_amd64", "x86_64-pc-windows-msvc"),
    "arm64": ("recall_windows_arm64", "aarch64-pc-windows-msvc"),
}


def old_style(version):
    """True for a version released with the old names; a pre-release counts
    as the version it leads up to."""
    core = re.split(r"[-+]", version.lstrip("v"))[0]
    parts = tuple(int(p) for p in core.split("."))
    if len(parts) != 3:
        sys.exit(f"not a release version: {version}")
    return parts <= LAST_OLD_STYLE_RELEASE


def archives(version):
    """winget architecture -> (archive name, recall.exe's path inside it)."""
    if old_style(version):
        return {arch: (f"{old}.zip", "recall.exe") for arch, (old, _) in PLATFORMS.items()}
    return {
        arch: (f"recall-{target}.zip", f"recall-{target}\\recall.exe")
        for arch, (_, target) in PLATFORMS.items()
    }


def parse_checksums(text, version):
    """The two Windows archives' hashes from a release's checksums.txt.

    Mirrors scripts/update-formula.py's `rewrite`: refuse rather than write a
    manifest that is silently missing an architecture.
    """
    want = dict.fromkeys(name for name, _ in archives(version).values())
    for line in text.splitlines():
        parts = line.split()
        if len(parts) == 2 and parts[1] in want:
            want[parts[1]] = parts[0]

    missing = [name for name, digest in want.items() if not digest]
    if missing:
        sys.exit("checksums.txt has no line for: " + ", ".join(missing))
    return want


def version_dict(version):
    return {
        "PackageIdentifier": IDENTIFIER,
        "PackageVersion": version,
        "DefaultLocale": DEFAULT_LOCALE,
        "ManifestType": "version",
        "ManifestVersion": SCHEMA_VERSION,
    }


def installer_dict(version, digests):
    """The installer manifest.

    recall.exe's path inside the zip is the same for both architectures in
    the old layout, so it is said once, for every installer. In the new one
    it names the architecture's own directory, so each installer carries
    its own NestedInstallerFiles, which the schema allows per installer.
    """
    per_arch = archives(version)
    shared = len({inner for _, inner in per_arch.values()}) == 1

    def nested(inner):
        return [{"RelativeFilePath": inner, "PortableCommandAlias": "recall"}]

    manifest = {
        "PackageIdentifier": IDENTIFIER,
        "PackageVersion": version,
        "InstallerType": "zip",
        "NestedInstallerType": "portable",
    }
    if shared:
        manifest["NestedInstallerFiles"] = nested(next(iter(per_arch.values()))[1])
    installers = []
    for arch, (archive, inner) in per_arch.items():
        installer = {"Architecture": arch}
        if not shared:
            installer["NestedInstallerFiles"] = nested(inner)
        installer["InstallerUrl"] = f"{REPO}/releases/download/v{version}/{archive}"
        installer["InstallerSha256"] = digests[archive].upper()
        installers.append(installer)
    manifest.update(
        {
            "Commands": ["recall"],
            "Installers": installers,
            "ManifestType": "installer",
            "ManifestVersion": SCHEMA_VERSION,
        }
    )
    return manifest


def default_locale_dict(version):
    return {
        "PackageIdentifier": IDENTIFIER,
        "PackageVersion": version,
        "PackageLocale": DEFAULT_LOCALE,
        "Publisher": PUBLISHER,
        "PublisherUrl": "https://github.com/pimlabs",
        "PublisherSupportUrl": f"{REPO}/issues",
        "PackageName": PACKAGE_NAME,
        "PackageUrl": REPO,
        "License": LICENSE,
        "LicenseUrl": f"{REPO}/blob/main/LICENSE",
        "ShortDescription": SHORT_DESCRIPTION,
        "Moniker": MONIKER,
        "Tags": TAGS,
        "ManifestType": "defaultLocale",
        "ManifestVersion": SCHEMA_VERSION,
    }


def render_yaml(manifest, schema_name):
    """A manifest dict as winget-pkgs' own manifests are formatted.

    Deliberately not a general-purpose YAML dumper — just enough to render
    the fixed, known-safe shape the three manifests above produce: dicts of
    strings, lists of strings, and lists of dicts (an installer's own
    NestedInstallerFiles is one inside another), none of which need
    quoting. Driven straight from the dict, rather than a separately
    hand-written template, so rendered text and validated data cannot drift
    apart — packaging/winget/test_manifest.py still round-trips the output
    through a real YAML parser to catch anything that slips past that
    assumption.
    """
    lines = [
        f"# yaml-language-server: $schema=https://aka.ms/winget-manifest.{schema_name}.{SCHEMA_VERSION}.schema.json",
        "",
    ]
    lines += _mapping(manifest, "")
    return "\n".join(lines) + "\n"


def _mapping(mapping, pad):
    lines = []
    for key, value in mapping.items():
        if isinstance(value, list):
            lines.append(f"{pad}{key}:")
            lines += _sequence(value, pad)
        else:
            lines.append(f"{pad}{key}: {value}")
    return lines


def _sequence(items, pad):
    lines = []
    for item in items:
        if isinstance(item, dict):
            # A dict item is a mapping two spaces in, whose first line
            # carries the dash.
            inner = _mapping(item, pad + "  ")
            inner[0] = f"{pad}- {inner[0][len(pad) + 2:]}"
            lines += inner
        else:
            lines.append(f"{pad}- {item}")
    return lines


def write_manifests(version, checksums_text, out_dir):
    digests = parse_checksums(checksums_text, version)
    first_letter, publisher_segment = IDENTIFIER[0].lower(), IDENTIFIER.split(".")[0]
    target = out_dir / "manifests" / first_letter / publisher_segment / PACKAGE_NAME / version
    target.mkdir(parents=True, exist_ok=True)

    (target / f"{IDENTIFIER}.yaml").write_text(render_yaml(version_dict(version), "version"))
    (target / f"{IDENTIFIER}.installer.yaml").write_text(
        render_yaml(installer_dict(version, digests), "installer")
    )
    (target / f"{IDENTIFIER}.locale.{DEFAULT_LOCALE}.yaml").write_text(
        render_yaml(default_locale_dict(version), "defaultLocale")
    )
    return target


def main():
    if len(sys.argv) not in (3, 4):
        sys.exit(__doc__.split("\n\n")[1])
    version, checksums_path = sys.argv[1], sys.argv[2]
    out_dir = Path(sys.argv[3]) if len(sys.argv) == 4 else Path("packaging/winget/out") / version

    checksums_text = Path(checksums_path).read_text()
    target = write_manifests(version, checksums_text, out_dir)
    print(f"wrote 3 manifest files to {target}")


if __name__ == "__main__":
    main()
