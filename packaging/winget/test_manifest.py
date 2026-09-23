#!/usr/bin/env python3
"""packaging/winget/generate_manifest.py, checked without a network or a release.

Needs `jsonschema` and `PyYAML` (CI installs both; see .github/workflows/ci.yml).
Everything else is offline: packaging/winget/fixtures/checksums.txt stands in
for a release's real one (its hashes are not real archive digests — they only
need to be syntactically valid — see that file's header), and the JSON
schemas are the vendored copies in packaging/winget/schemas/, not a live
fetch from microsoft/winget-cli. See that directory's SOURCE.txt for where
they came from.

Three things per manifest, so a bug in the hand-rolled YAML renderer and a
schema violation don't look like the same failure:

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

import subprocess
import sys
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

FIXTURE_VERSION = "0.9.9"
FIXTURE_CHECKSUMS = (ROOT / "fixtures" / "checksums.txt").read_text()

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
    import json

    return json.loads(SCHEMAS[kind].read_text())


def round_trip(kind, manifest_dict):
    """Render, parse back, validate. Returns the parsed dict."""
    text = gm.render_yaml(manifest_dict, kind)
    parsed = yaml.safe_load(text)
    check(f"{kind}: rendered YAML parses back to the same dict", parsed == manifest_dict)

    schema = load_schema(kind)
    validator = Draft7Validator(schema)
    errors = sorted(validator.iter_errors(parsed), key=str)
    check(
        f"{kind}: validates against manifest.{kind}.{gm.SCHEMA_VERSION}.json",
        not errors,
    )
    for e in errors:
        print(f"  schema error in {kind}: {e.message} (at {'/'.join(str(p) for p in e.path)})")
    return parsed


def main():
    digests = gm.parse_checksums(FIXTURE_CHECKSUMS)

    round_trip("version", gm.version_dict(FIXTURE_VERSION))
    installer_parsed = round_trip("installer", gm.installer_dict(FIXTURE_VERSION, digests))
    round_trip("defaultLocale", gm.default_locale_dict(FIXTURE_VERSION))

    # The two architectures are really both there, not just schema-shaped.
    archs = {i["Architecture"] for i in installer_parsed["Installers"]}
    check("installer: both Windows architectures present", archs == {"x64", "arm64"})
    check(
        "installer: NestedInstallerFiles carries the portable alias",
        installer_parsed["NestedInstallerFiles"] == [
            {"RelativeFilePath": "recall.exe", "PortableCommandAlias": "recall"}
        ],
    )

    # write_manifests(), end to end, into a real temp directory.
    import tempfile

    with tempfile.TemporaryDirectory() as tmp:
        target = gm.write_manifests(FIXTURE_VERSION, FIXTURE_CHECKSUMS, Path(tmp))
        check(
            "write_manifests: three files, in winget-pkgs' own layout",
            target == Path(tmp) / "manifests" / "p" / "PimLabs" / "Recall" / FIXTURE_VERSION
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
            check(f"write_manifests: {name} parses as YAML", isinstance(
                yaml.safe_load((target / name).read_text()), dict
            ))

    # A checksums.txt missing one Windows archive: refused, via the CLI —
    # sys.exit inside the function under test would also end *this* script.
    short = "\n".join(
        line for line in FIXTURE_CHECKSUMS.splitlines() if "windows_arm64" not in line
    )
    with tempfile.TemporaryDirectory() as tmp:
        checksums_path = Path(tmp) / "checksums.txt"
        checksums_path.write_text(short)
        result = subprocess.run(
            [sys.executable, str(ROOT / "generate_manifest.py"), FIXTURE_VERSION, str(checksums_path), tmp],
            capture_output=True,
            text=True,
        )
        check("missing architecture: generate_manifest.py refuses", result.returncode != 0)
        check(
            "missing architecture: says which one",
            "recall_windows_arm64.zip" in (result.stdout + result.stderr),
        )
        check(
            "missing architecture: nothing written",
            not (Path(tmp) / "manifests").exists(),
        )

    print(f"passed {pass_count}, failed {fail_count}")
    sys.exit(1 if fail_count else 0)


if __name__ == "__main__":
    main()
