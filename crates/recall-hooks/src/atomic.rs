//! Write-then-rename, used everywhere Recall touches a file that something
//! else might be reading at the same moment.
//!
//! Two readers make this non-negotiable: a session starting mid-pull would
//! otherwise read a truncated memory file, and two push hooks racing on
//! adjacent edits would otherwise leave a truncated baseline. The shell
//! version wrote both with a plain `>` redirect and could do exactly that.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

/// Writes `contents` to `path` through a temp file in the same directory
/// (same filesystem, so the rename is atomic) and renames it into place.
///
/// The temp file is named with `prefix`/`suffix` so a stray one is
/// recognisable, and is removed on drop if the rename never happens.
pub(crate) fn write(path: &Path, prefix: &str, suffix: &str, contents: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;

    let mut tmp = tempfile::Builder::new()
        .prefix(prefix)
        .suffix(suffix)
        .tempfile_in(dir)?;
    tmp.write_all(contents)?;
    tmp.flush()?;

    // 0o600 is tempfile's default; these are a user's own notes and settings
    // sitting in their repo, and the rest of the tree is 0o644.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o644))?;
    }

    persist(tmp, path)
}

/// Renames the temp file over `path`.
///
/// On Unix a rename replaces the target whoever has it open. Windows
/// refuses with "access denied" while any other handle is open on the
/// target without delete sharing: another push hook reading the baseline
/// at that moment, which Claude Code's parallel hooks make routine, or an
/// antivirus scanner looking at a file that was just written. Both let go
/// within milliseconds, so the rename is retried with a short backoff,
/// about 1.3 s in all, before the error is reported. Anywhere else a
/// refused rename is a real permission problem and reported at once.
fn persist(tmp: tempfile::NamedTempFile, path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    let tmp = {
        let mut tmp = tmp;
        let mut delay = std::time::Duration::from_millis(5);
        for _ in 0..8 {
            match tmp.persist(path) {
                Ok(_) => return Ok(()),
                Err(e) if e.error.kind() == io::ErrorKind::PermissionDenied => {
                    tmp = e.file;
                    std::thread::sleep(delay);
                    delay *= 2;
                }
                Err(e) => return Err(e.error),
            }
        }
        tmp
    };
    tmp.persist(path).map(|_| ()).map_err(|e| e.error)
}
