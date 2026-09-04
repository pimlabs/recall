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

    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}
