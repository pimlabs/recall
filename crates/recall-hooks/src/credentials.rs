//! The credentials file `recall connect` writes: `~/.recall/credentials.json`.
//!
//! It exists because the only other place a token could live was the
//! environment, and a token in the environment has three specific exposures:
//! every subprocess inherits it — including the postinstall script of any
//! package in any project you touch — a shell profile is among the most
//! commonly published files there are, and `export RECALL_TOKEN=…` typed
//! once stays in shell history for good.
//!
//! The shape follows `cargo login`: one file, keyed per server, with room
//! for a keychain later without touching any caller. Cargo's *machinery* —
//! provider lists, a credential-process protocol — is for many registries
//! and many organisations, and is not copied.
//!
//! It sits **below** every environment layer. An explicit `RECALL_TOKEN`
//! still wins, and that is right rather than a compromise: a cloud
//! environment's variables and a CI system's secrets *are* secret stores.
//! The environment is correct where something else already holds the secret
//! properly; this file is correct where the alternative was a human pasting
//! a token into a dotfile.
//!
//! Mode `0600` is a Unix guarantee. On Windows the file is written the same
//! way and nothing restricts it — Windows is not a supported client yet,
//! and this says so rather than implying a protection that is not there.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Overrides where the credentials live, the way `CARGO_HOME` does for
/// cargo. It is also what keeps tests off the real home directory.
pub const HOME_VAR: &str = "RECALL_HOME";

/// The file's name inside [`home`].
pub const FILE_NAME: &str = "credentials.json";

/// The format written today. Carried in the file from the first version,
/// because it is the difference between migrating a file later and guessing
/// what it meant.
pub const VERSION: u32 = 1;

/// Why the file could not be used.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// It exists and could not be read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// The file.
        path: String,
        /// Why.
        source: io::Error,
    },
    /// It was read and is not a credentials file this version understands.
    #[error("{path} is not a credentials file Recall can read: {reason}")]
    Parse {
        /// The file.
        path: String,
        /// What was wrong with it.
        reason: String,
    },
}

/// One server's entry.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Server {
    /// The bearer token.
    pub token: String,
}

/// The whole file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Credentials {
    /// Always [`VERSION`] when written by this build.
    pub version: u32,
    /// The server used when `RECALL_URL` is not set anywhere — the one most
    /// recently connected. Normalised, like every key in `servers`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Tokens, keyed by normalised server URL, so connecting to a second
    /// server cannot silently reuse the first one's token.
    #[serde(default)]
    pub servers: BTreeMap<String, Server>,
}

impl Default for Credentials {
    fn default() -> Self {
        Self {
            version: VERSION,
            default: None,
            servers: BTreeMap::new(),
        }
    }
}

impl Credentials {
    /// The token saved for `url`, which is normalised first.
    pub fn token_for(&self, url: &str) -> Option<&str> {
        self.servers
            .get(&normalize_url(url))
            .map(|s| s.token.as_str())
            .filter(|t| !t.is_empty())
    }

    /// Saves `token` for `url` and makes it the default.
    pub fn insert(&mut self, url: &str, token: &str) {
        let url = normalize_url(url);
        self.servers.insert(
            url.clone(),
            Server {
                token: token.to_string(),
            },
        );
        self.default = Some(url);
    }

    /// Forgets `url`. Returns whether there was anything to forget.
    ///
    /// Clears the default when it pointed here, rather than promoting some
    /// other server to it: which server a machine talks to is not something
    /// to change on the user's behalf as a side effect of removing one.
    pub fn remove(&mut self, url: &str) -> bool {
        let url = normalize_url(url);
        if self.default.as_deref() == Some(url.as_str()) {
            self.default = None;
        }
        self.servers.remove(&url).is_some()
    }
}

/// The directory credentials live in: `RECALL_HOME`, else `~/.recall`.
///
/// Read through the same lookup as every other setting, so a settings file
/// can point it elsewhere and a test can without touching the process
/// environment. [`None`] when there is no home directory to put it in.
pub fn home<F>(lookup: F) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(dir) = lookup(HOME_VAR).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    lookup("HOME")
        .filter(|v| !v.is_empty())
        .map(|h| Path::new(&h).join(".recall"))
}

/// The credentials file under `home`.
pub fn file(home: &Path) -> PathBuf {
    home.join(FILE_NAME)
}

/// One spelling per server.
///
/// `https://x.id`, `https://x.id/` and `https://x.id ` would otherwise be
/// three different keys, and the "no token" that results is
/// indistinguishable from "wrong token". Applied on write *and* on read,
/// which is the only way the two agree. Scheme and host are lowercased
/// because they are case-insensitive; the path is left alone because it is
/// not.
pub fn normalize_url(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let (host, path) = match rest.find('/') {
                Some(i) => rest.split_at(i),
                None => (rest, ""),
            };
            format!(
                "{}://{}{}",
                scheme.to_ascii_lowercase(),
                host.to_ascii_lowercase(),
                path
            )
        }
        None => url.to_string(),
    }
}

/// Reads the file. `Ok(None)` when there is none, which is the ordinary
/// state of every machine that has not run `recall connect`.
pub fn load(path: &Path) -> Result<Option<Credentials>, Error> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(Error::Read {
                path: path.display().to_string(),
                source,
            })
        }
    };
    let creds: Credentials = serde_json::from_slice(&bytes).map_err(|e| Error::Parse {
        path: path.display().to_string(),
        reason: e.to_string(),
    })?;
    // A newer Recall may have written something this one would misread. Say
    // so rather than guess — and rather than let `connect` overwrite it.
    if creds.version != VERSION {
        return Err(Error::Parse {
            path: path.display().to_string(),
            reason: format!(
                "version {} (this build understands {VERSION})",
                creds.version
            ),
        });
    }
    Ok(Some(creds))
}

/// Writes the file atomically, readable by its owner only.
///
/// The temp file is created in the same directory — so the rename cannot
/// cross a filesystem — and created `0600` rather than chmodded after, so
/// there is no moment at which the token sits in a file anyone else can
/// read. The directory is `0700` for the same reason.
pub fn save(path: &Path, creds: &Credentials) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    create_private_dir(dir)?;

    let mut body = serde_json::to_vec_pretty(creds).map_err(io::Error::other)?;
    body.push(b'\n');

    // `tempfile` creates with 0600 on Unix, before a byte is written.
    let mut tmp = tempfile::Builder::new()
        .prefix(".credentials-")
        .suffix(".tmp")
        .tempfile_in(dir)?;
    tmp.write_all(&body)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Removes the file. Not an error when it is already gone.
pub fn delete(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Whether someone other than the owner can read the file.
///
/// `recall connect` never writes one like that, but a copy, a restore from a
/// backup or a hand edit can. Always `false` off Unix, where there are no
/// mode bits to read — see the module documentation.
pub fn readable_by_others(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o077 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        match fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
        {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(e),
        }
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn home_prefers_recall_home_then_dot_recall_under_home() {
        assert_eq!(
            home(env(&[("RECALL_HOME", "/r"), ("HOME", "/h")])),
            Some(PathBuf::from("/r"))
        );
        assert_eq!(
            home(env(&[("HOME", "/h")])),
            Some(PathBuf::from("/h/.recall"))
        );
        // Empty counts as unset, as it does for every other variable.
        assert_eq!(
            home(env(&[("RECALL_HOME", ""), ("HOME", "/h")])),
            Some(PathBuf::from("/h/.recall"))
        );
        assert_eq!(home(env(&[])), None);
    }

    /// Three spellings of one server have to be one key, or a token saved
    /// under one is "missing" under the others — and missing looks exactly
    /// like wrong.
    #[test]
    fn spellings_of_one_server_normalise_to_one_key() {
        for url in [
            "https://recall.example.com",
            "https://recall.example.com/",
            "https://recall.example.com//",
            "  https://recall.example.com \n",
            "HTTPS://Recall.Example.COM",
        ] {
            assert_eq!(normalize_url(url), "https://recall.example.com", "{url:?}");
        }
        // The path is case-sensitive and survives.
        assert_eq!(
            normalize_url("https://Example.com/Recall/"),
            "https://example.com/Recall"
        );
    }

    #[test]
    fn a_token_saved_under_one_spelling_is_found_under_another() {
        let mut c = Credentials::default();
        c.insert("https://x.example.com/", "t");
        assert_eq!(c.token_for("HTTPS://X.example.com"), Some("t"));
        assert_eq!(c.default.as_deref(), Some("https://x.example.com"));
    }

    /// Per-server keying is the point: a second server must not be handed
    /// the first one's token.
    #[test]
    fn a_second_server_does_not_get_the_first_ones_token() {
        let mut c = Credentials::default();
        c.insert("https://a.example.com", "ta");
        assert_eq!(c.token_for("https://b.example.com"), None);
        c.insert("https://b.example.com", "tb");
        assert_eq!(c.token_for("https://a.example.com"), Some("ta"));
        assert_eq!(c.default.as_deref(), Some("https://b.example.com"));
    }

    #[test]
    fn removing_the_default_clears_it_rather_than_picking_another() {
        let mut c = Credentials::default();
        c.insert("https://a.example.com", "ta");
        c.insert("https://b.example.com", "tb");
        assert!(c.remove("https://b.example.com/"));
        assert_eq!(c.default, None);
        assert!(!c.remove("https://b.example.com"), "already gone");
        assert_eq!(c.token_for("https://a.example.com"), Some("ta"));
    }

    #[test]
    fn a_missing_file_is_none_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(&dir.path().join("credentials.json"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("credentials.json");
        let mut c = Credentials::default();
        c.insert("https://x.example.com", "s3cret");
        save(&path, &c).unwrap();
        assert_eq!(load(&path).unwrap(), Some(c));
    }

    /// The whole reason the file exists instead of a dotfile line.
    #[cfg(unix)]
    #[test]
    fn the_file_is_owner_only_and_so_is_its_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(".recall");
        let path = file(&home);
        save(&path, &Credentials::default()).unwrap();

        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&home), 0o700);
        assert!(!readable_by_others(&path));

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(readable_by_others(&path));
    }

    /// A rename over the old file, not a truncate-and-write into it: nothing
    /// named like a temp file may be left behind, and the old contents are
    /// replaced whole.
    #[test]
    fn save_replaces_the_file_and_leaves_no_temp_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let mut c = Credentials::default();
        c.insert("https://a.example.com", "one");
        save(&path, &c).unwrap();
        c.insert("https://a.example.com", "two");
        save(&path, &c).unwrap();

        assert_eq!(
            load(&path)
                .unwrap()
                .unwrap()
                .token_for("https://a.example.com"),
            Some("two")
        );
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["credentials.json"]);
    }

    /// A file this build cannot read must be an error, not an empty store —
    /// otherwise `connect` would overwrite whatever it actually held.
    #[test]
    fn an_unreadable_or_future_file_is_an_error_not_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");

        fs::write(&path, "not json").unwrap();
        assert!(matches!(load(&path), Err(Error::Parse { .. })));

        fs::write(&path, r#"{"version":2,"servers":{}}"#).unwrap();
        let err = load(&path).unwrap_err();
        assert!(err.to_string().contains("version 2"), "{err}");
    }
}
