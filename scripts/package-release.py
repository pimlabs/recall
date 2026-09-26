#!/usr/bin/env python3
"""Pack one release archive, the way every release from 0.4.6 on is packed.

Usage: package-release.py <binary> <archive name> <output dir>

  package-release.py target/x86_64-apple-darwin/release/recall \\
      recall-x86_64-apple-darwin.tar.gz dist

The archive holds one top-level directory named like the archive without
its extension, and in it the binary under its own name plus LICENSE and
README.md from the repository root, as uv and ripgrep ship theirs:

  recall-x86_64-apple-darwin/recall
  recall-x86_64-apple-darwin/LICENSE
  recall-x86_64-apple-darwin/README.md

`.tar.gz` or `.zip`, chosen by the archive name. Python rather than `tar`
and PowerShell's Compress-Archive so the six client archives and the two
server ones come out of one piece of code on every runner, and so CI can
run exactly this code to build the archives its installer test serves
(scripts/installer-test.sh), rather than a copy of it. zipfile also writes
`/` between path components on Windows, which the zip format requires and
Windows PowerShell 5.1's Compress-Archive did not.

v0.4.5 and older were packed differently (a bare binary renamed like the
archive, or a bare recall.exe in the zip); those archives are never
rebuilt, so this script knows nothing about them.
"""

import sys
import tarfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
EXTRAS = ("LICENSE", "README.md")
SUFFIXES = (".tar.gz", ".zip")


def members(binary: Path):
    """(path on disk, name inside the directory, mode) for each file."""
    yield binary, binary.name, 0o755
    for name in EXTRAS:
        yield ROOT / name, name, 0o644


def pack(binary: Path, archive_name: str, out_dir: Path) -> Path:
    suffix = next((s for s in SUFFIXES if archive_name.endswith(s)), None)
    if suffix is None or "/" in archive_name or "\\" in archive_name:
        sys.exit(f"package-release: {archive_name} is not a bare name ending in .tar.gz or .zip")
    if not binary.is_file():
        sys.exit(f"package-release: no binary at {binary}")
    top = archive_name[: -len(suffix)]
    out_dir.mkdir(parents=True, exist_ok=True)
    archive = out_dir / archive_name

    if suffix == ".tar.gz":
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
            for path, name, mode in members(binary):
                tar.add(path, arcname=f"{top}/{name}", filter=lambda i, m=mode: tidy(i, m))
    else:
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as z:
            directory = zipfile.ZipInfo(f"{top}/")
            directory.external_attr = (0o40755 << 16) | 0x10
            z.writestr(directory, b"")
            for path, name, mode in members(binary):
                info = zipfile.ZipInfo.from_file(path, f"{top}/{name}")
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
    if len(sys.argv) != 4:
        sys.exit(__doc__.split("\n\n")[1])
    archive = pack(Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3]))
    print(archive)
    for name in listing(archive):
        print(f"  {name}")


if __name__ == "__main__":
    main()
