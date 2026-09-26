#!/usr/bin/env python3
"""packaging/winget/generate_manifest.py, checked without a network or a release.

Needs `jsonschema` and `PyYAML` (CI installs both; see .github/workflows/ci.yml).
Everything else is offline: packaging/winget/fixtures/ stands in for two
releases' real checksums.txt (their hashes are not real archive digests —
they only need to be syntactically valid): checksums.txt with the archive
names every release after v0.4.5 uses, checksums-0.4.5.txt with the names
v0.4.5 and older used. The JSON schemas are the vendored copies in
packaging/winget/schemas/, not a live fetch from microsoft/winget-cli. See
that directory's SOURCE.txt for where they came from.

Three things per manifest, for a release on each side of the rename, so a
bug in the hand-rolled YAML renderer and a schema violation don't look like
the same failure:

1. generate_manifest's dict and its rendered text agree once the text is
   parsed back by a real YAML parser — catches a rendering bug (bad
   indentation, a value that needed quoting and didn't get it).
2. The rendered text validates against the vendored schema — catches a
   manifest shape winget-pkgs would reject.
3. A checksums.txt missing one of the two Windows archives is refused,
   exactly like scripts/update-formula-check.sh checks for the Homebrew
   formula rewrite — a manifest silently missing an architecture is worse
   than the run failing.
"""

import json
import subprocess
import sys
import tempfile
from pathlib import Path

import yaml
from jsonschema import Draft7Validator

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT))
import generate_manifest as gm  # noqa: E402

SCHEMAS = {
    "version": ROOT / "schemas" / f"manifest.version.{gm.SCHEMA_VERSION}.json",
    "installer": ROOT / "schemas" / f"manifest.installer.{gm.SCHEMA_VERSION}.json",
    "defaultLocale": ROOT / "schemas" / f"manifest.defaultLocale.{gm.SCHEMA_VERSION}.json",
}

# (version, its checksums.txt, recall.exe's path per architecture, the
# arm64 archive's name), for a release on each side of the rename.
CASES = (
    (
        "0.9.9",
        (ROOT / "fixtures" / "checksums.txt").read_text(),
        {
            "x64": "recall-x86_64-pc-windows-msvc\\recall.exe",
            "arm64": "recall-aarch64-pc-windows-msvc\\recall.exe",
        },
        "recall-aarch64-pc-windows-msvc.zip",
    ),
    (
        "0.4.5",
        (ROOT / "fixtures" / "checksums-0.4.5.txt").read_text(),
        {"x64": "recall.exe", "arm64": "recall.exe"},
        "recall_windows_arm64.zip",
    ),
)

pass_count = 0
fail_count = 0


def check(name, ok):
    global pass_count, fail_count
    if ok:
        pass_count += 1
    else:
        fail_count += 1
        print(f"FAIL: {name}")


def load_schema(kind):
    return json.loads(SCHEMAS[kind].read_text())


def round_trip(label, kind, manifest_dict):
    """Render, parse back, validate. Returns the parsed dict."""
    text = gm.render_yaml(manifest_dict, kind)
    parsed = yaml.safe_load(text)
    check(f"{label} {kind}: rendered YAML parses back to the same dict", parsed == manifest_dict)

    schema = load_schema(kind)
    validator = Draft7Validator(schema)
    errors = sorted(validator.iter_errors(parsed), key=str)
    check(
        f"{label} {kind}: validates against manifest.{kind}.{gm.SCHEMA_VERSION}.json",
        not errors,
    )
    for e in errors:
        print(f"  schema error in {kind}: {e.message} (at {'/'.join(str(p) for p in e.path)})")
    return parsed


def exe_paths(label, installer):
    """winget architecture -> the RelativeFilePath that applies to it, and
    every alias, wherever in the manifest each is said."""
    paths, aliases = {}, set()
    for i in installer["Installers"]:
        nested = i.get("NestedInstallerFiles", installer.get("NestedInstallerFiles"))
        check(
            f"{label} installer: {i['Architecture']} has exactly one nested file",
            nested is not None and len(nested) == 1,
        )
        if nested:
            paths[i["Architecture"]] = nested[0]["RelativeFilePath"]
            aliases.add(nested[0].get("PortableCommandAlias"))
    return paths, aliases


def refused(version, checksums_text):
    """Runs the CLI (sys.exit inside the function under test would also end
    *this* script) and returns (refused, output, wrote anything)."""
    with tempfile.TemporaryDirectory() as tmp:
        checksums_path = Path(tmp) / "checksums.txt"
        checksums_path.write_text(checksums_text)
        result = subprocess.run(
            [sys.executable, str(ROOT / "generate_manifest.py"), version, str(checksums_path), tmp],
            capture_output=True,
            text=True,
        )
        return (
            result.returncode != 0,
            result.stdout + result.stderr,
            (Path(tmp) / "manifests").exists(),
        )


def check_release(version, checksums, want_paths, arm64_archive):
    label = f"v{version}:"
    digests = gm.parse_checksums(checksums, version)

    round_trip(label, "version", gm.version_dict(version))
    installer = round_trip(label, "installer", gm.installer_dict(version, digests))
    round_trip(label, "defaultLocale", gm.default_locale_dict(version))

    # The two architectures are really both there, not just schema-shaped,
    # each pointing into its own archive at the file that archive holds.
    archs = {i["Architecture"] for i in installer["Installers"]}
    check(f"{label} installer: both Windows architectures present", archs == {"x64", "arm64"})
    paths, aliases = exe_paths(label, installer)
    check(f"{label} installer: recall.exe's path in each archive", paths == want_paths)
    check(f"{label} installer: the portable alias is recall", aliases == {"recall"})
    urls = {i["Architecture"]: i["InstallerUrl"] for i in installer["Installers"]}
    check(
        f"{label} installer: arm64 downloads {arm64_archive}",
        urls["arm64"].endswith(f"/v{version}/{arm64_archive}"),
    )

    # write_manifests(), end to end, into a real temp directory.
    with tempfile.TemporaryDirectory() as tmp:
        target = gm.write_manifests(version, checksums, Path(tmp))
        check(
            f"{label} write_manifests: three files, in winget-pkgs' own layout",
            target == Path(tmp) / "manifests" / "p" / "PimLabs" / "Recall" / version
            and sorted(p.name for p in target.iterdir())
            == [
                "PimLabs.Recall.installer.yaml",
                "PimLabs.Recall.locale.en-US.yaml",
                "PimLabs.Recall.yaml",
            ],
        )
        for name in (
            "PimLabs.Recall.yaml",
            "PimLabs.Recall.installer.yaml",
            "PimLabs.Recall.locale.en-US.yaml",
        ):
            check(
                f"{label} write_manifests: {name} parses as YAML",
                isinstance(yaml.safe_load((target / name).read_text()), dict),
            )

    # A checksums.txt missing one Windows archive: refused, naming it, and
    # nothing written.
    short = "\n".join(line for line in checksums.splitlines() if arm64_archive not in line)
    no, output, wrote = refused(version, short)
    check(f"{label} missing architecture: generate_manifest.py refuses", no)
    check(f"{label} missing architecture: says which one", arm64_archive in output)
    check(f"{label} missing architecture: nothing written", not wrote)


def main():
    for case in CASES:
        check_release(*case)

    # The other side's checksums.txt is refused: a release after v0.4.5 that
    # still carried the old names, or the reverse, is not what this version
    # published.
    for version, _, _, _ in CASES:
        other = next(c for c in CASES if c[0] != version)[1]
        no, _, wrote = refused(version, other)
        check(f"v{version}: the other naming's checksums.txt is refused", no and not wrote)

    print(f"passed {pass_count}, failed {fail_count}")
    sys.exit(1 if fail_count else 0)


if __name__ == "__main__":
    main()
