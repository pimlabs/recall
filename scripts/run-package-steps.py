#!/usr/bin/env python3
"""Run release.yml's own packaging steps against fake binaries.

Usage: run-package-steps.py <release.yml> <work dir> <version> <out dir>

For scripts/installer-test.sh, which serves what this writes as a release
and installs from it. The point is that the archives are named and packed
by release.yml's own `run:` text, not by a copy of it in the test: for each
entry of the `build` and `build-server` matrices, every step whose name
starts with "Package" and whose `if:` holds for that entry's runner is run
with bash, `${{ matrix.* }}` filled in from the entry, in a scratch
checkout holding a fake binary at target/<target>/release/<binary>. What
lands in its dist/ is moved to <out dir>.

The fakes are shell scripts that print `<binary> <version> <target>`, so an
installer that picks the wrong archive is caught by what it installed. The
server's also print the `features: ... passkeys` line the server's Package
step insists on. `file` and `readelf`, which that step asks about the
binary, are stand-ins that report nothing wrong, and `python` (what the
Windows step calls) is this python3.

Refuses anything it cannot run faithfully: an expression other than
`${{ matrix.<key> }}` of the entry, a step `if:` other than a comparison of
`runner.os`, a shell other than bash, and a matrix entry that no Package
step packages. Prints `<job> <target> <archive>` for each archive written.
"""

import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

import yaml

REPO = Path(__file__).resolve().parent.parent
# What each job's `cargo build -p` makes, and so what a Package step finds.
BINARIES = {
    "build": ("recall",),
    "build-server": ("recall-server", "recall-worker"),
}
STAND_INS = """\
python() { python3 "$@"; }
file() { echo "$1: (a stand-in for the real binary)"; }
readelf() { :; }
"""


def runner_os(entry):
    os_label = entry["os"]
    if os_label.startswith("windows"):
        return "Windows"
    if os_label.startswith("macos"):
        return "macOS"
    return "Linux"


def applies(step, entry):
    cond = step.get("if")
    if cond is None:
        return True
    m = re.fullmatch(r"\s*runner\.os\s*(==|!=)\s*'(\w+)'\s*", cond)
    if not m:
        sys.exit(f"run-package-steps: cannot evaluate `if: {cond}` on step {step.get('name')}")
    return (runner_os(entry) == m.group(2)) == (m.group(1) == "==")


def fill(text, entry, step_name):
    def value(m):
        key = m.group(1)
        if key not in entry:
            sys.exit(f"run-package-steps: {step_name} uses matrix.{key}, which the entry {entry} lacks")
        return str(entry[key])

    text = re.sub(r"\$\{\{\s*matrix\.([A-Za-z0-9_-]+)\s*\}\}", value, text)
    if "${{" in text:
        sys.exit(f"run-package-steps: {step_name} uses an expression this cannot fill in")
    return text


def fake(path, lines):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("#!/bin/sh\n" + "".join(f'echo "{line}"\n' for line in lines))
    path.chmod(0o755)


def main():
    if len(sys.argv) != 5:
        sys.exit(__doc__.split("\n\n")[1])
    workflow, work, version, out = sys.argv[1], Path(sys.argv[2]), sys.argv[3], Path(sys.argv[4])
    jobs = yaml.safe_load(open(workflow))["jobs"]
    out.mkdir(parents=True, exist_ok=True)

    for job, binaries in BINARIES.items():
        steps = [s for s in jobs[job]["steps"] if str(s.get("name", "")).startswith("Package")]
        if not steps:
            sys.exit(f"run-package-steps: {job} has no Package step")
        for entry in jobs[job]["strategy"]["matrix"]["include"]:
            target = entry["target"]
            root = work / f"{job}-{target}"
            shutil.rmtree(root, ignore_errors=True)
            root.mkdir(parents=True)
            # The checkout the step runs in: the real scripts/, fake binaries.
            (root / "scripts").symlink_to(REPO / "scripts")
            for binary in binaries:
                exe = binary + (".exe" if runner_os(entry) == "Windows" else "")
                lines = [f"{binary} {version} {target}"]
                if binary == "recall-server":
                    lines.append("features: passkeys")
                fake(root / "target" / target / "release" / exe, lines)

            ran = 0
            for step in steps:
                if not applies(step, entry):
                    continue
                shell = step.get("shell", "bash" if runner_os(entry) != "Windows" else "pwsh")
                if shell != "bash":
                    sys.exit(f"run-package-steps: {job}'s {step['name']} runs in {shell}; only bash is run here")
                script = STAND_INS + fill(step["run"], entry, step["name"])
                result = subprocess.run(
                    ["bash", "-e", "-c", script],
                    cwd=root,
                    env=dict(os.environ),
                    capture_output=True,
                    text=True,
                )
                if result.returncode != 0:
                    sys.stderr.write(result.stdout + result.stderr)
                    sys.exit(f"run-package-steps: {job} {target}: {step['name']} failed")
                ran += 1
            if ran == 0:
                sys.exit(f"run-package-steps: no Package step in {job} applies to {target}")

            dist = root / "dist"
            for archive in sorted(dist.iterdir()) if dist.is_dir() else []:
                shutil.move(str(archive), out / archive.name)
                print(job, target, archive.name)


if __name__ == "__main__":
    main()
