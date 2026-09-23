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

# One installer per architecture, each a zip holding a bare recall.exe — the
# same archives .github/workflows/release.yml's `build` job publishes (see
# its "Package (Windows)" step) and the same names docs/reference/install.md
# documents. Keep this in step with both if either ever changes.
ARCHIVES = {
    "x64": "recall_windows_amd64.zip",
    "arm64": "recall_windows_arm64.zip",
}


def parse_checksums(text):
    """The two Windows archives' hashes from a release's checksums.txt.

    Mirrors scripts/update-formula.py's `rewrite`: refuse rather than write a
    manifest that is silently missing an architecture.
    """
    want = dict.fromkeys(ARCHIVES.values())
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
    return {
        "PackageIdentifier": IDENTIFIER,
        "PackageVersion": version,
        "InstallerType": "zip",
        "NestedInstallerType": "portable",
        "NestedInstallerFiles": [
            {"RelativeFilePath": "recall.exe", "PortableCommandAlias": "recall"},
        ],
        "Commands": ["recall"],
        "Installers": [
            {
                "Architecture": arch,
                "InstallerUrl": f"{REPO}/releases/download/v{version}/{archive}",
                "InstallerSha256": digests[archive].upper(),
            }
            for arch, archive in ARCHIVES.items()
        ],
        "ManifestType": "installer",
        "ManifestVersion": SCHEMA_VERSION,
    }


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
    the fixed, known-safe shape the three manifests above produce: a flat
    dict of strings, lists of strings, and one-level lists of dicts, none of
    which need quoting. Driven straight from the dict, rather than a
    separately hand-written template, so rendered text and validated data
    cannot drift apart — packaging/winget/test_manifest.py still round-trips
    the output through a real YAML parser to catch anything that slips past
    that assumption.
    """
    lines = [
        f"# yaml-language-server: $schema=https://aka.ms/winget-manifest.{schema_name}.{SCHEMA_VERSION}.schema.json",
        "",
    ]
    for key, value in manifest.items():
        if isinstance(value, list):
            lines.append(f"{key}:")
            for item in value:
                if isinstance(item, dict):
                    first = True
                    for k2, v2 in item.items():
                        lines.append(f"{'- ' if first else '  '}{k2}: {v2}")
                        first = False
                else:
                    lines.append(f"- {item}")
        else:
            lines.append(f"{key}: {value}")
    return "\n".join(lines) + "\n"


def write_manifests(version, checksums_text, out_dir):
    digests = parse_checksums(checksums_text)
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
