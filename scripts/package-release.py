#!/usr/bin/env python3
"""Pack one release archive, the way every release from 0.4.6 on is packed.

Usage: package-release.py <binary> <rust target> <output dir>
       package-release.py --name <binary name> <rust target>

  package-release.py target/x86_64-apple-darwin/release/recall x86_64-apple-darwin dist
    -> dist/recall-x86_64-apple-darwin.tar.gz

The archive's name is decided here and nowhere else: `<binary>-<target>`,
where <binary> is the file's name without `.exe`, as a `.zip` for a Windows
target and a `.tar.gz` for every other. release.yml passes only the binary
and the target; `--name` prints the name without packing anything, for
anything else that needs to know it (scripts/installer-test.sh).

The archive holds one top-level directory named like the archive without
its extension, and in it the binary under its own name plus LICENSE and
README.md from the repository root, as uv and ripgrep ship theirs:

  recall-x86_64-apple-darwin/recall
  recall-x86_64-apple-darwin/LICENSE
  recall-x86_64-apple-darwin/README.md

Python rather than `tar` and PowerShell's Compress-Archive so the six client
archives and the four server and worker ones come out of one piece of code
on every runner, and so CI runs exactly this code, from release.yml's own
steps, to build the release its installer test serves
(scripts/installer-test.sh). zipfile also writes `/` between path
components on Windows, which the zip format requires and Windows
PowerShell 5.1's Compress-Archive did not.

v0.4.5 and older were packed differently (a bare binary renamed like the
archive, or a bare recall.exe in the zip); those archives are never
rebuilt, so this script knows nothing about them.
"""

import re
import sys
import tarfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
EXTRAS = ("LICENSE", "README.md")
BINARIES = ("recall", "recall-server", "recall-worker")


def archive_name(binary_name: str, target: str) -> str:
    """The one place a release archive's file name is made."""
    stem = binary_name[: -len(".exe")] if binary_name.endswith(".exe") else binary_name
    if stem not in BINARIES:
        sys.exit(f"package-release: {binary_name} is not a binary a release publishes")
    if not re.fullmatch(r"[a-z0-9_]+(-[a-z0-9_]+){2,3}", target):
        sys.exit(f"package-release: {target} is not a Rust target triple")
    ext = "zip" if "-windows-" in target else "tar.gz"
    return f"{stem}-{target}.{ext}"


def members(binary: Path):
    """(path on disk, name inside the directory, mode) for each file."""
    yield binary, binary.name, 0o755
    for name in EXTRAS:
        yield ROOT / name, name, 0o644


def pack(binary: Path, target: str, out_dir: Path) -> Path:
    if not binary.is_file():
        sys.exit(f"package-release: no binary at {binary}")
    name = archive_name(binary.name, target)
    top = name[: -len(".zip")] if name.endswith(".zip") else name[: -len(".tar.gz")]
    out_dir.mkdir(parents=True, exist_ok=True)
    archive = out_dir / name

    if name.endswith(".tar.gz"):
        def tidy(info: tarfile.TarInfo, mode: int) -> tarfile.TarInfo:
            # Nobody's user or group from the build machine, and modes that
            # do not depend on its umask.
            info.uid = info.gid = 0
            info.uname = info.gname = ""
            info.mode = mode
            return info

        with tarfile.open(archive, "w:gz", format=tarfile.PAX_FORMAT) as tar:
            directory = tarfile.TarInfo(top)
            directory.type = tarfile.DIRTYPE
            tar.addfile(tidy(directory, 0o755))
            for path, member, mode in members(binary):
                tar.add(path, arcname=f"{top}/{member}", filter=lambda i, m=mode: tidy(i, m))
    else:
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as z:
            directory = zipfile.ZipInfo(f"{top}/")
            directory.external_attr = (0o40755 << 16) | 0x10
            z.writestr(directory, b"")
            for path, member, mode in members(binary):
                info = zipfile.ZipInfo.from_file(path, f"{top}/{member}")
                info.compress_type = zipfile.ZIP_DEFLATED
                info.external_attr = (0o100000 | mode) << 16
                z.writestr(info, path.read_bytes())
    return archive


def listing(archive: Path) -> list:
    if archive.name.endswith(".zip"):
        with zipfile.ZipFile(archive) as z:
            return z.namelist()
    with tarfile.open(archive) as tar:
        return tar.getnames()


def main() -> None:
    usage = "\n".join(__doc__.split("\n\n")[1].splitlines()[:2])
    if len(sys.argv) == 4 and sys.argv[1] == "--name":
        print(archive_name(sys.argv[2], sys.argv[3]))
        return
    if len(sys.argv) != 4 or sys.argv[1].startswith("-"):
        sys.exit(usage)
    archive = pack(Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3]))
    print(archive)
    for name in listing(archive):
        print(f"  {name}")


if __name__ == "__main__":
    main()
