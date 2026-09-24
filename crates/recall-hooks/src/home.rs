//! `~/.recall`: the files that make up one machine's Recall setup.
//!
//! ```text
//! ~/.recall/config.toml        0644  server, machine name — safe to read, edit, back up
//! ~/.recall/credentials.toml   0600  one token per server — written by `recall connect`
//! ~/.recall/device.key         0600  this machine's device keys, one per server
//! ~/.recall/audit.json         0644  checkpoints of each server's audit log, see `crate::audit`
//! ```
//!
//! `device.key` holds what a machine enrolled as a device signs its requests
//! with (see `docs/design/handshake.md`). It is a file, not the OS keychain,
//! and on purpose: `recall push` runs on every memory write, and a keychain
//! can stop a hook to show a dialog nobody is there to answer. macOS does
//! exactly that when the binary asking changes, which an upgrade does. The
//! file is created `0600` inside the `0700` directory, which is the same
//! protection `gh` and Claude Code give their own tokens on Linux.
//!
//! Two files rather than one because they are handled differently, not
//! because they describe different things. The config is something a person
//! opens, edits, and keeps in a dotfiles repository; the credentials are
//! never meant to be opened at all, and keeping them apart means naming this
//! machine never involves a file with a secret in it. `cargo` splits
//! `config.toml` from `credentials.toml` for the same reason.
//!
//! Everything here sits **below** the environment. An explicit `RECALL_URL`,
//! `RECALL_TOKEN`, `RECALL_SOURCE_ENV` or `RECALL_MACHINE_KEY` still wins —
//! a cloud environment's variables and CI's secrets are the right store
//! there — and `recall status` says which one is in effect.
//!
//! Mode `0600` is a Unix guarantee. On Windows the files are written the same
//! way but nothing restricts them — there is no mode-bit equivalent to check
//! or set — so [`readable_by_others`] always answers `false` there rather
//! than implying a protection that is not present, and `recall doctor` does
//! not warn about it on that platform. What protects them there is where
//! they are: `%USERPROFILE%\.recall`, inside the user's profile, whose
//! access list Windows sets to the user, SYSTEM and Administrators, and
//! which every file created in it inherits. A `RECALL_HOME` outside the
//! profile has no such protection, and [`inside_profile`] is how `recall
//! doctor` knows not to claim it.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Overrides where these files live, the way `CARGO_HOME` does for cargo.
/// It is also what keeps tests off the real home directory.
pub const HOME_VAR: &str = "RECALL_HOME";

/// The format both files are written in today.
///
/// It is the version of the *file*, not of Recall. A future Recall that
/// changes a file's shape knows from this which shape it is reading and can
/// migrate it; an older Recall meeting a newer file refuses to read it rather
/// than misreading it and then writing over what it did not understand.
pub const VERSION: u32 = 1;

const CONFIG_FILE: &str = "config.toml";
const CREDENTIALS_FILE: &str = "credentials.toml";
/// What 0.3.0 wrote: both the default server and the tokens, in JSON.
const LEGACY_FILE: &str = "credentials.json";
const DEVICE_FILE: &str = "device.key";

const CONFIG_HEADER: &str = "\
# Recall's settings for this machine. Safe to read, edit and back up.
# The token lives in credentials.toml next to this, readable by you only.
# Anything set in the environment (RECALL_URL, RECALL_MACHINE_KEY, ...) wins
# over this file; `recall status` says which one is in effect.
";

const CREDENTIALS_HEADER: &str = "\
# Written by `recall connect`. Readable by you only.
# Do not edit, commit or share this file; `recall disconnect` removes an entry.
";

const DEVICE_HEADER: &str = "\
# This machine's device keys, written by `recall connect`. Readable by you only.
# Each private key was made on this machine and never leaves it: do not copy,
# commit or share this file. `recall disconnect` removes an entry.
";

/// Why a file could not be used.
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
    /// It was read and is not something this version understands.
    #[error("{path} is not a file Recall can read: {reason}")]
    Parse {
        /// The file.
        path: String,
        /// What was wrong with it.
        reason: String,
    },
    /// Writing it failed.
    #[error("cannot write {path}: {source}")]
    Write {
        /// The file.
        path: String,
        /// Why.
        source: io::Error,
    },
}

fn default_version() -> u32 {
    VERSION
}

/// `config.toml`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Config {
    /// The file's format version; see [`VERSION`].
    #[serde(default = "default_version")]
    pub version: u32,
    /// The server this machine talks to when `RECALL_URL` is not set. Its
    /// token is looked up in `credentials.toml` under this URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// About this machine.
    #[serde(default, skip_serializing_if = "Machine::is_empty")]
    pub machine: Machine,
    /// Keys this version does not know. Kept rather than refused, so a
    /// newer Recall can add a key without an older one failing on it — and
    /// reported by `recall doctor`, so a misspelt key does not silently do
    /// nothing.
    #[serde(flatten, skip_serializing)]
    pub unknown: BTreeMap<String, toml::Value>,
}

/// `[machine]` in `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Machine {
    /// This machine's name. One value, used twice: it labels every file
    /// this machine syncs (what `RECALL_SOURCE_ENV` used to be for), and it
    /// names this machine's own scope, `machine:<name>` (what
    /// `RECALL_MACHINE_KEY` used to be for). They were two variables that
    /// had to agree and nothing checked that they did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// As for [`Config::unknown`].
    #[serde(flatten, skip_serializing)]
    pub unknown: BTreeMap<String, toml::Value>,
}

impl Machine {
    fn is_empty(&self) -> bool {
        self.name.is_none()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: VERSION,
            server: None,
            machine: Machine::default(),
            unknown: BTreeMap::new(),
        }
    }
}

impl Config {
    /// Keys in the file this version does not recognise, as dotted paths.
    pub fn unknown_keys(&self) -> Vec<String> {
        let mut out: Vec<String> = self.unknown.keys().cloned().collect();
        out.extend(self.machine.unknown.keys().map(|k| format!("machine.{k}")));
        out
    }
}

/// One server's entry in `credentials.toml`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Server {
    /// The bearer token.
    pub token: String,
}

/// `credentials.toml`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Credentials {
    /// The file's format version; see [`VERSION`].
    #[serde(default = "default_version")]
    pub version: u32,
    /// Tokens, keyed by normalised server URL, so connecting to a second
    /// server cannot silently reuse the first one's token — and switching
    /// back to the first does not need its token again.
    #[serde(default)]
    pub servers: BTreeMap<String, Server>,
}

impl Default for Credentials {
    fn default() -> Self {
        Self {
            version: VERSION,
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

    /// Saves `token` for `url`.
    pub fn insert(&mut self, url: &str, token: &str) {
        self.servers.insert(
            normalize_url(url),
            Server {
                token: token.to_string(),
            },
        );
    }

    /// Forgets `url`. Returns whether there was anything to forget.
    pub fn remove(&mut self, url: &str) -> bool {
        self.servers.remove(&normalize_url(url)).is_some()
    }
}

/// One server's entry in `device.key`: the device this machine is there,
/// and the key it signs with.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DeviceEntry {
    /// `dev_…`, the `keyid` every signature names.
    pub device_id: String,
    /// The name the server knows it by, which a push it signs is stored
    /// under.
    pub name: String,
    /// `sync` or `admin`, as the server said when it was approved.
    pub scope: String,
    /// Whether the server removes it once idle: a cloud session enrolled
    /// with an authkey.
    #[serde(default)]
    pub ephemeral: bool,
    /// The Ed25519 private key's 32-byte seed, base64url without padding.
    pub private_key: String,
}

/// Never prints the private key: a configuration is logged and printed in
/// test failures, and this is the one secret that must not travel.
impl std::fmt::Debug for DeviceEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceEntry")
            .field("device_id", &self.device_id)
            .field("name", &self.name)
            .field("scope", &self.scope)
            .field("ephemeral", &self.ephemeral)
            .field("private_key", &"(hidden)")
            .finish()
    }
}

/// `device.key`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Devices {
    /// The file's format version; see [`VERSION`].
    #[serde(default = "default_version")]
    pub version: u32,
    /// One device per server, keyed by normalised URL like the tokens: an
    /// enrolment belongs to the server that approved it, and each server
    /// gets its own key, so re-enrolling at one never touches another.
    #[serde(default)]
    pub servers: BTreeMap<String, DeviceEntry>,
}

impl Default for Devices {
    fn default() -> Self {
        Self {
            version: VERSION,
            servers: BTreeMap::new(),
        }
    }
}

impl Devices {
    /// The device saved for `url`, which is normalised first.
    pub fn for_url(&self, url: &str) -> Option<&DeviceEntry> {
        self.servers.get(&normalize_url(url))
    }

    /// Saves `entry` for `url`, replacing any earlier one.
    pub fn insert(&mut self, url: &str, entry: DeviceEntry) {
        self.servers.insert(normalize_url(url), entry);
    }

    /// Forgets `url`. Returns whether there was anything to forget.
    pub fn remove(&mut self, url: &str) -> bool {
        self.servers.remove(&normalize_url(url)).is_some()
    }
}

/// The `~/.recall` directory and the files in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Home {
    dir: PathBuf,
}

/// Where `~/.recall` is: `RECALL_HOME`, else `$HOME/.recall`, else
/// `%USERPROFILE%\.recall` on a machine with no `HOME` — Windows does not set
/// one by default. [`None`] when there is no home directory to put it in.
///
/// Read through the same lookup as every other setting, so a settings file
/// can point it elsewhere and a test can without touching the process
/// environment.
pub fn locate<F>(lookup: F) -> Option<Home>
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(dir) = lookup(HOME_VAR).filter(|v| !v.is_empty()) {
        return Some(Home::at(dir));
    }
    lookup("HOME")
        .filter(|v| !v.is_empty())
        .or_else(|| lookup("USERPROFILE").filter(|v| !v.is_empty()))
        .map(|h| Home::at(Path::new(&h).join(".recall")))
}

impl Home {
    /// The files in `dir`.
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory itself.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `config.toml`.
    pub fn config_path(&self) -> PathBuf {
        self.dir.join(CONFIG_FILE)
    }

    /// `credentials.toml`.
    pub fn credentials_path(&self) -> PathBuf {
        self.dir.join(CREDENTIALS_FILE)
    }

    /// `device.key`.
    pub fn device_path(&self) -> PathBuf {
        self.dir.join(DEVICE_FILE)
    }

    /// Reads `device.key`. `Ok(None)` when there is none: a machine that
    /// uses the shared token, or has not connected at all.
    pub fn load_devices(&self) -> Result<Option<Devices>, Error> {
        load_toml(&self.device_path())
    }

    /// Writes `device.key` atomically, readable by its owner only, and
    /// created that way rather than narrowed afterwards, like the
    /// credentials. An empty store removes the file instead.
    pub fn save_devices(&self, devices: &Devices) -> Result<(), Error> {
        if devices.servers.is_empty() {
            return match fs::remove_file(self.device_path()) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(Error::Write {
                    path: self.device_path().display().to_string(),
                    source: e,
                }),
                _ => Ok(()),
            };
        }
        let body = toml::to_string(devices).map_err(|e| Error::Write {
            path: self.device_path().display().to_string(),
            source: io::Error::other(e),
        })?;
        write_atomic(&self.device_path(), DEVICE_HEADER, &body, 0o600)
    }

    /// Saves `entry` as this machine's device at `url`, keeping every other
    /// server's. Takes [`Home::lock_devices`] for the read and the write.
    pub fn save_device(&self, url: &str, entry: DeviceEntry) -> Result<(), Error> {
        self.lock_devices()?.save_device(url, entry)
    }

    /// Forgets the device saved for `url`. Returns whether there was one.
    /// Takes [`Home::lock_devices`] for the read and the write.
    pub fn forget_device(&self, url: &str) -> Result<bool, Error> {
        self.lock_devices()?.forget_device(url)
    }

    /// Waits for, and takes, the lock every change to `device.key` is made
    /// under, until the [`DevicesLock`] is dropped.
    ///
    /// `device.key` is changed by reading it, changing one server's entry
    /// and writing the whole file back. Two processes doing that at once
    /// lose one change, and hooks do run at once: Claude Code starts one per
    /// edit, and a cloud session's first few edits can all find no device
    /// key and each enrol one. Holding this across "is there a key, enrol,
    /// save" makes the second find the first one's key instead.
    ///
    /// The lock is a file created only if absent, `device.key.lock`, the
    /// way `audit.json`'s is: no OS file lock, which the standard library
    /// gained after this workspace's Rust. One left behind by a process
    /// killed while holding it is taken over once it is older than anything
    /// holds one on purpose.
    pub fn lock_devices(&self) -> Result<DevicesLock<'_>, Error> {
        Ok(DevicesLock {
            home: self,
            _lock: FileLock::take(&self.dir, DEVICE_LOCK_FILE, LOCK_WAIT)?,
        })
    }

    /// `audit.json`: the checkpoints of each server's audit log this
    /// machine has seen, and what checking them found. See
    /// [`crate::audit`].
    pub fn audit_path(&self) -> PathBuf {
        self.dir.join(crate::audit::AUDIT_FILE)
    }

    /// `credentials.json`, which 0.3.0 wrote and [`Home::migrate_legacy`]
    /// retires.
    pub fn legacy_path(&self) -> PathBuf {
        self.dir.join(LEGACY_FILE)
    }

    /// Reads `config.toml`. `Ok(None)` when there is none — the ordinary
    /// state of a machine that has not run `recall connect`.
    pub fn load_config(&self) -> Result<Option<Config>, Error> {
        load_toml(&self.config_path())
    }

    /// Reads `credentials.toml`, likewise.
    pub fn load_credentials(&self) -> Result<Option<Credentials>, Error> {
        load_toml(&self.credentials_path())
    }

    /// Writes `config.toml` atomically. Mode `0644`: it holds nothing secret,
    /// and a file its owner can read but a backup tool cannot is one that
    /// does not get backed up.
    pub fn save_config(&self, config: &Config) -> Result<(), Error> {
        let body = toml::to_string(config).map_err(|e| Error::Write {
            path: self.config_path().display().to_string(),
            source: io::Error::other(e),
        })?;
        write_atomic(&self.config_path(), CONFIG_HEADER, &body, 0o644)
    }

    /// Writes `credentials.toml` atomically, readable by its owner only.
    ///
    /// Created `0600` rather than chmodded after, so there is no moment at
    /// which the token sits in a file anyone else can read.
    pub fn save_credentials(&self, creds: &Credentials) -> Result<(), Error> {
        let body = toml::to_string(creds).map_err(|e| Error::Write {
            path: self.credentials_path().display().to_string(),
            source: io::Error::other(e),
        })?;
        write_atomic(&self.credentials_path(), CREDENTIALS_HEADER, &body, 0o600)
    }

    /// Removes `credentials.toml`. Not an error when it is already gone.
    pub fn delete_credentials(&self) -> Result<(), Error> {
        match fs::remove_file(self.credentials_path()) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(Error::Write {
                path: self.credentials_path().display().to_string(),
                source: e,
            }),
            _ => Ok(()),
        }
    }

    /// Moves what 0.3.0's `credentials.json` held into the two TOML files,
    /// then removes it. Returns whether there was anything to move.
    ///
    /// Written before removed, so an interruption leaves both and the next
    /// run finishes the job; merging is idempotent, and nothing already in
    /// the new files is overwritten by the old one.
    pub fn migrate_legacy(&self) -> Result<bool, Error> {
        let Some((server, old)) = self.read_legacy()? else {
            return Ok(false);
        };

        let mut creds = self.load_credentials()?.unwrap_or_default();
        for (url, entry) in old.servers {
            creds.servers.entry(url).or_insert(entry);
        }
        self.save_credentials(&creds)?;

        let mut config = self.load_config()?.unwrap_or_default();
        if config.server.is_none() && server.is_some() {
            config.server = server;
            self.save_config(&config)?;
        }

        fs::remove_file(self.legacy_path()).map_err(|source| Error::Write {
            path: self.legacy_path().display().to_string(),
            source,
        })?;
        Ok(true)
    }

    /// `credentials.json` as (default server, tokens), when it exists.
    ///
    /// Read by [`Home::migrate_legacy`], and by configuration loading while
    /// a migration has not happened — so a migration that fails never
    /// leaves a working machine without its token.
    pub fn read_legacy(&self) -> Result<Option<(Option<String>, Credentials)>, Error> {
        #[derive(serde::Deserialize)]
        struct Legacy {
            version: u32,
            #[serde(default)]
            default: Option<String>,
            #[serde(default)]
            servers: BTreeMap<String, Server>,
        }

        let path = self.legacy_path();
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(Error::Read {
                    path: path.display().to_string(),
                    source,
                })
            }
        };
        let old: Legacy = serde_json::from_slice(&bytes).map_err(|e| Error::Parse {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        if old.version != 1 {
            return Err(Error::Parse {
                path: path.display().to_string(),
                reason: format!("version {} (only version 1 is migrated)", old.version),
            });
        }
        let creds = Credentials {
            version: VERSION,
            servers: old
                .servers
                .into_iter()
                .map(|(url, s)| (normalize_url(&url), s))
                .collect(),
        };
        Ok(Some((old.default.map(|u| normalize_url(&u)), creds)))
    }
}

/// `device.key.lock`, beside the file it guards.
const DEVICE_LOCK_FILE: &str = "device.key.lock";

/// How old a lock must be to be taken over as left behind. What is done
/// under it is at most one enrolment, a single request with a sixty-second
/// timeout, and some file writes.
const LOCK_STALE: std::time::Duration = std::time::Duration::from_secs(90);

/// How long to wait for the lock before giving up: long enough to outlast
/// a lock left behind, which is taken over once stale.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(100);

/// How often a process waiting for the lock looks again.
const LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// `device.key`, held by this process until dropped: see
/// [`Home::lock_devices`]. Everything that changes the file while holding
/// it reads it afresh first, so nothing another process saved is lost.
#[derive(Debug)]
pub struct DevicesLock<'a> {
    home: &'a Home,
    _lock: FileLock,
}

impl DevicesLock<'_> {
    /// `device.key` as it is now, empty when there is none.
    pub fn load(&self) -> Result<Devices, Error> {
        Ok(self.home.load_devices()?.unwrap_or_default())
    }

    /// Saves `entry` as this machine's device at `url`, keeping every other
    /// server's.
    pub fn save_device(&self, url: &str, entry: DeviceEntry) -> Result<(), Error> {
        let mut devices = self.load()?;
        devices.insert(url, entry);
        self.home.save_devices(&devices)
    }

    /// Forgets the device saved for `url`, and no other. Returns whether
    /// there was one.
    pub fn forget_device(&self, url: &str) -> Result<bool, Error> {
        let mut devices = self.load()?;
        let removed = devices.remove(url);
        if removed {
            self.home.save_devices(&devices)?;
        }
        Ok(removed)
    }
}

/// A lock file in `~/.recall`, held by this process until dropped: what
/// [`Home::lock_devices`] takes for `device.key`, and [`crate::audit`] for
/// `audit.json`.
///
/// A lock file created with `O_EXCL` rather than an OS file lock: the
/// standard library's is newer than this workspace's Rust, and this needs
/// no crate for what a file created only-if-absent already does. What that
/// costs is a lock left behind by a process killed while holding it, which
/// is why one older than `LOCK_STALE` is taken over: nothing holds one that
/// long on purpose.
#[derive(Debug)]
pub(crate) struct FileLock {
    path: PathBuf,
}

impl FileLock {
    /// Waits up to `wait` for the lock file `name` in `dir`, and takes it.
    pub(crate) fn take(dir: &Path, name: &str, wait: std::time::Duration) -> Result<Self, Error> {
        let path = dir.join(name);
        let err = |source| Error::Write {
            path: path.display().to_string(),
            source,
        };
        create_private_dir(dir).map_err(err)?;
        let deadline = std::time::Instant::now() + wait;
        let mut denied = 0;
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(FileLock { path }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    denied = 0;
                    let stale = fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|at| at.elapsed().ok())
                        .is_some_and(|age| age > LOCK_STALE);
                    if stale {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    if std::time::Instant::now() >= deadline {
                        return Err(err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("another recall held {name} for too long"),
                        )));
                    }
                    std::thread::sleep(LOCK_POLL);
                }
                // Windows refuses to create a file whose name belongs to one
                // still being deleted, which the lock is for a moment after
                // its holder lets go if anything had it open then. That is
                // the lock being busy, not a permission problem, but only
                // briefly: a directory this user really cannot write in is
                // reported after a few refusals in a row rather than waited
                // on for the whole wait.
                Err(e)
                    if cfg!(windows)
                        && e.kind() == io::ErrorKind::PermissionDenied
                        && denied < 20 =>
                {
                    denied += 1;
                    std::thread::sleep(LOCK_POLL);
                }
                Err(e) => return Err(err(e)),
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// A machine name as `config.toml` may hold it, or [`None`] if it cannot be
/// one.
///
/// Letters, digits, `.`, `-` and `_`, up to 64 of them — the characters a
/// hostname is made of, so the default offered from one needs no escaping.
/// A leading `machine:` is dropped: the scope key is derived from the name,
/// and a name that already carries the prefix is what someone copying it
/// out of `recall status` would type.
///
/// ```
/// # use recall_hooks::home::machine_name;
/// assert_eq!(machine_name(" jarvis "), Some("jarvis".into()));
/// assert_eq!(machine_name("machine:jarvis"), Some("jarvis".into()));
/// assert_eq!(machine_name("my laptop"), None);
/// assert_eq!(machine_name(""), None);
/// ```
pub fn machine_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    let name = name.strip_prefix("machine:").unwrap_or(name);
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    ok.then(|| name.to_string())
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

/// Whether someone other than the owner can read `path`.
///
/// `recall connect` never writes a credentials file like that, but a copy,
/// a restore from a backup or a hand edit can. Always `false` off Unix,
/// where there are no mode bits to read — see the module documentation.
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

/// Makes `path` readable by its owner only, when anyone else could read it.
/// `Ok(true)` when it had to. Always `Ok(false)` off Unix, for the reason
/// [`readable_by_others`] always answers `false` there.
pub fn restrict_to_owner(path: &Path) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)?.permissions().mode();
        if mode & 0o077 == 0 {
            return Ok(false);
        }
        fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o700))?;
        Ok(true)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(false)
    }
}

/// Whether `path` is inside `profile`, the user's profile directory
/// (`%USERPROFILE%`), compared the way Windows compares paths: component by
/// component, and ignoring case.
///
/// On Windows that is what protects the files here, not mode bits: the
/// profile's access list, which covers only what is inside it. A
/// `RECALL_HOME` elsewhere gets whatever that directory's list says, and
/// nothing here reads it, so nothing may claim the profile's protection for
/// it.
pub fn inside_profile(path: &Path, profile: &str) -> bool {
    let profile = profile.trim();
    if profile.is_empty() {
        return false;
    }
    let lower = |p: &str| p.to_lowercase();
    Path::new(&lower(&path.to_string_lossy())).starts_with(lower(profile))
}

fn load_toml<T: serde::de::DeserializeOwned + HasVersion>(path: &Path) -> Result<Option<T>, Error> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(Error::Read {
                path: path.display().to_string(),
                source,
            })
        }
    };
    let value: T = toml::from_str(&text).map_err(|e| Error::Parse {
        path: path.display().to_string(),
        reason: e.message().to_string(),
    })?;
    if value.version() != VERSION {
        return Err(Error::Parse {
            path: path.display().to_string(),
            reason: format!(
                "version {} (this build understands {VERSION}), nothing was changed",
                value.version()
            ),
        });
    }
    Ok(Some(value))
}

trait HasVersion {
    fn version(&self) -> u32;
}

impl HasVersion for Config {
    fn version(&self) -> u32 {
        self.version
    }
}

impl HasVersion for Credentials {
    fn version(&self) -> u32 {
        self.version
    }
}

impl HasVersion for Devices {
    fn version(&self) -> u32 {
        self.version
    }
}

fn write_atomic(path: &Path, header: &str, body: &str, mode: u32) -> Result<(), Error> {
    let err = |source| Error::Write {
        path: path.display().to_string(),
        source,
    };
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    create_private_dir(dir).map_err(err)?;

    // `tempfile` creates with 0600 on Unix, before a byte is written; the
    // config is then widened, never the credentials narrowed after the fact.
    let mut tmp = tempfile::Builder::new()
        .prefix(".recall-")
        .suffix(".tmp")
        .tempfile_in(dir)
        .map_err(err)?;
    tmp.write_all(header.as_bytes()).map_err(err)?;
    tmp.write_all(b"\n").map_err(err)?;
    tmp.write_all(body.as_bytes()).map_err(err)?;
    tmp.as_file().sync_all().map_err(err)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if mode != 0o600 {
            fs::set_permissions(tmp.path(), fs::Permissions::from_mode(mode)).map_err(err)?;
        }
    }
    #[cfg(not(unix))]
    let _ = mode;
    tmp.persist(path).map_err(|e| err(e.error))?;
    Ok(())
}

/// `0700`: the directory holds a secret, and its listing is nobody else's
/// business either.
///
/// One that already exists is narrowed to that too, when it is wider: made
/// by hand, restored from a backup or unpacked from a dotfiles repository,
/// it can be `0755`. Narrowing is best effort, because `RECALL_HOME` may name
/// a directory this user does not own, and a write there has never failed
/// for that reason.
fn create_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        match fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
        {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
            _ => {}
        }
        if let Ok(meta) = fs::metadata(dir) {
            let mode = meta.permissions().mode();
            if mode & 0o077 != 0 {
                let _ = fs::set_permissions(dir, fs::Permissions::from_mode(mode & 0o700));
            }
        }
        Ok(())
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
            locate(env(&[("RECALL_HOME", "/r"), ("HOME", "/h")])),
            Some(Home::at("/r"))
        );
        assert_eq!(locate(env(&[("HOME", "/h")])), Some(Home::at("/h/.recall")));
        assert_eq!(
            locate(env(&[("RECALL_HOME", ""), ("HOME", "/h")])),
            Some(Home::at("/h/.recall"))
        );
        assert_eq!(locate(env(&[])), None);
    }

    /// Windows has no `HOME` by default, so `USERPROFILE` has to stand in.
    /// Not verified against a real Windows machine — see `claude.rs`.
    ///
    /// The expected paths are built with `Path::join` rather than typed as
    /// `C:\Users\eko\.recall` literals: this test runs on Linux too, where
    /// `\` is an ordinary filename character, not a separator, so a
    /// hand-written backslash path would compare a string this crate never
    /// actually produces against the one it does.
    #[test]
    fn home_falls_back_to_userprofile_when_home_is_unset() {
        let want = Home::at(Path::new(r"C:\Users\eko").join(".recall"));
        assert_eq!(locate(env(&[("USERPROFILE", r"C:\Users\eko")])), Some(want));
        let want = Home::at(Path::new(r"C:\Users\eko").join(".recall"));
        assert_eq!(
            locate(env(&[("HOME", ""), ("USERPROFILE", r"C:\Users\eko")])),
            Some(want),
            "HOME declared empty reads as unset, same as everywhere else"
        );
        assert_eq!(
            locate(env(&[("HOME", "/h"), ("USERPROFILE", r"C:\Users\eko")])),
            Some(Home::at("/h/.recall")),
            "HOME wins when both are set"
        );
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
    }

    /// Per-server keying is the point: a second server must not be handed
    /// the first one's token, and the first keeps its own.
    #[test]
    fn each_server_keeps_its_own_token() {
        let mut c = Credentials::default();
        c.insert("https://a.example.com", "ta");
        assert_eq!(c.token_for("https://b.example.com"), None);
        c.insert("https://b.example.com", "tb");
        assert_eq!(c.token_for("https://a.example.com"), Some("ta"));
        assert!(c.remove("https://b.example.com/"));
        assert!(!c.remove("https://b.example.com"), "already gone");
    }

    #[test]
    fn missing_files_are_none_not_errors() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path());
        assert!(home.load_config().unwrap().is_none());
        assert!(home.load_credentials().unwrap().is_none());
        assert!(!home.migrate_legacy().unwrap());
    }

    #[test]
    fn both_files_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("nested"));
        let config = Config {
            server: Some("https://x.example.com".into()),
            machine: Machine {
                name: Some("jarvis".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut creds = Credentials::default();
        creds.insert("https://x.example.com", "s3cret");
        home.save_config(&config).unwrap();
        home.save_credentials(&creds).unwrap();

        assert_eq!(home.load_config().unwrap(), Some(config));
        assert_eq!(home.load_credentials().unwrap(), Some(creds));
    }

    /// What someone opening the file sees, which is part of the design:
    /// a comment saying what the file is, then keys they can read.
    #[test]
    fn the_config_file_reads_as_intended() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path());
        home.save_config(&Config {
            server: Some("https://x.example.com".into()),
            machine: Machine {
                name: Some("jarvis".into()),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        let text = fs::read_to_string(home.config_path()).unwrap();
        assert!(text.starts_with("# Recall's settings"), "{text}");
        assert!(text.contains("version = 1\n"), "{text}");
        assert!(
            text.contains("server = \"https://x.example.com\"\n"),
            "{text}"
        );
        assert!(text.contains("[machine]\nname = \"jarvis\"\n"), "{text}");
        assert!(
            !text.contains("token ="),
            "no secret in the config file: {text}"
        );
    }

    /// The whole reason there are two files.
    #[cfg(unix)]
    #[test]
    fn the_credentials_are_owner_only_and_the_config_is_not() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join(".recall"));
        home.save_credentials(&Credentials::default()).unwrap();
        home.save_config(&Config::default()).unwrap();

        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&home.credentials_path()), 0o600);
        assert_eq!(mode(&home.config_path()), 0o644);
        assert_eq!(mode(home.dir()), 0o700);
        assert!(!readable_by_others(&home.credentials_path()));
    }

    fn entry(id: &str) -> DeviceEntry {
        DeviceEntry {
            device_id: id.to_string(),
            name: id.to_string(),
            scope: "sync".to_string(),
            ephemeral: false,
            private_key: "c2VlZA".to_string(),
        }
    }

    /// One key per server: saving or forgetting one server's device never
    /// touches another's.
    #[test]
    fn each_server_keeps_its_own_device_key() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path());
        home.save_device("https://a.example.com", entry("dev_a"))
            .unwrap();
        home.save_device("https://b.example.com/", entry("dev_b"))
            .unwrap();
        home.save_device("https://a.example.com", entry("dev_a2"))
            .unwrap();

        let devices = home.load_devices().unwrap().unwrap();
        assert_eq!(
            devices
                .for_url("https://a.example.com")
                .map(|d| d.device_id.as_str()),
            Some("dev_a2")
        );
        assert_eq!(
            devices
                .for_url("https://b.example.com")
                .map(|d| d.device_id.as_str()),
            Some("dev_b")
        );

        assert!(home.forget_device("https://A.example.com").unwrap());
        let devices = home.load_devices().unwrap().unwrap();
        assert!(devices.for_url("https://a.example.com").is_none());
        assert!(devices.for_url("https://b.example.com").is_some());
        assert!(
            !dir.path().join(DEVICE_LOCK_FILE).exists(),
            "the lock goes with the change"
        );
    }

    /// Only one process at a time holds the lock: a second waits until the
    /// first lets go.
    #[test]
    fn the_device_lock_is_held_by_one_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join(".recall"));
        let held = home.lock_devices().unwrap();
        let released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let waiter = {
            let (home, released) = (home.clone(), released.clone());
            std::thread::spawn(move || {
                let _second = home.lock_devices().unwrap();
                released.load(std::sync::atomic::Ordering::SeqCst)
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(200));
        released.store(true, std::sync::atomic::Ordering::SeqCst);
        drop(held);
        assert!(
            waiter.join().unwrap(),
            "the second took the lock only once the first let it go"
        );
    }

    /// A lock left behind by a process that died holding it is taken over
    /// once it is older than anything holds it on purpose.
    #[test]
    fn a_lock_left_behind_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path());
        let left = fs::File::create(dir.path().join(DEVICE_LOCK_FILE)).unwrap();
        left.set_modified(std::time::SystemTime::now() - LOCK_STALE * 2)
            .unwrap();
        drop(left);
        let started = std::time::Instant::now();
        home.save_device("https://a.example.com", entry("dev_a"))
            .unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    /// A `~/.recall` made wider than `0700`, by hand or by a restore, is
    /// narrowed by the next write to it.
    #[cfg(unix)]
    #[test]
    fn an_existing_wide_directory_is_narrowed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let recall = dir.path().join(".recall");
        fs::create_dir(&recall).unwrap();
        fs::set_permissions(&recall, fs::Permissions::from_mode(0o755)).unwrap();
        Home::at(&recall).save_config(&Config::default()).unwrap();
        assert_eq!(
            fs::metadata(&recall).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_file_others_can_read_is_made_the_owners_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("device.key");
        fs::write(&file, "x").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(readable_by_others(&file));
        assert!(restrict_to_owner(&file).unwrap());
        assert!(!readable_by_others(&file));
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!restrict_to_owner(&file).unwrap(), "nothing left to do");
    }

    /// Inside the profile, whatever the case; a sibling whose name merely
    /// starts the same way is not inside it.
    #[test]
    fn only_a_path_inside_the_profile_is_inside_it() {
        let profile = "/home/eko";
        assert!(inside_profile(
            Path::new("/home/eko/.recall/device.key"),
            profile
        ));
        assert!(inside_profile(
            Path::new("/HOME/Eko/.recall/device.key"),
            profile
        ));
        assert!(!inside_profile(
            Path::new("/home/ekon/.recall/device.key"),
            profile
        ));
        assert!(!inside_profile(
            Path::new("/srv/recall/device.key"),
            profile
        ));
        assert!(!inside_profile(
            Path::new("/home/eko/.recall/device.key"),
            ""
        ));
    }

    /// Replaced whole by a rename; nothing named like a temp file is left.
    #[test]
    fn saving_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path());
        let mut c = Credentials::default();
        c.insert("https://a.example.com", "one");
        home.save_credentials(&c).unwrap();
        c.insert("https://a.example.com", "two");
        home.save_credentials(&c).unwrap();

        assert_eq!(
            home.load_credentials()
                .unwrap()
                .unwrap()
                .token_for("https://a.example.com"),
            Some("two")
        );
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["credentials.toml"]);
    }

    /// A file this build cannot read is an error, never an empty store —
    /// otherwise the next write would replace whatever it actually held.
    #[test]
    fn an_unreadable_or_future_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path());

        fs::write(home.credentials_path(), "servers = [").unwrap();
        assert!(matches!(home.load_credentials(), Err(Error::Parse { .. })));

        fs::write(home.config_path(), "version = 2\n").unwrap();
        let err = home.load_config().unwrap_err();
        assert!(err.to_string().contains("version 2"), "{err}");
    }

    /// Hand-written config: `version` may be left out, and a misspelt key is
    /// kept for `recall doctor` to name rather than refused or dropped.
    #[test]
    fn a_hand_written_config_is_accepted_and_its_typos_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path());
        fs::write(
            home.config_path(),
            "server = \"https://x.example.com\"\n[machine]\nnmae = \"jarvis\"\n",
        )
        .unwrap();
        let config = home.load_config().unwrap().unwrap();
        assert_eq!(config.version, VERSION);
        assert_eq!(config.machine.name, None);
        assert_eq!(config.unknown_keys(), ["machine.nmae"]);
    }

    #[test]
    fn machine_names_are_hostname_characters() {
        for ok in ["jarvis", "mbp-2", "work.laptop", "a_b", "machine:jarvis"] {
            assert!(machine_name(ok).is_some(), "{ok}");
        }
        for bad in ["", "  ", "my laptop", "a/b", "caf\u{e9}", &"x".repeat(65)] {
            assert_eq!(machine_name(bad), None, "{bad:?}");
        }
    }

    const LEGACY: &str = r#"{"version":1,"default":"https://A.example.com/","servers":{"https://a.example.com":{"token":"ta"}}}"#;

    /// 0.3.0's JSON file becomes the two TOML files and goes away — the
    /// default server into the config, the token into the credentials.
    #[test]
    fn the_0_3_0_file_is_migrated_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path());
        fs::write(home.legacy_path(), LEGACY).unwrap();

        assert!(home.migrate_legacy().unwrap());

        assert!(!home.legacy_path().exists());
        assert_eq!(
            home.load_config().unwrap().unwrap().server.as_deref(),
            Some("https://a.example.com")
        );
        assert_eq!(
            home.load_credentials()
                .unwrap()
                .unwrap()
                .token_for("https://a.example.com"),
            Some("ta")
        );
        assert!(!home.migrate_legacy().unwrap(), "nothing left to migrate");
    }

    /// What is already in the new files wins: the migration only fills gaps.
    #[test]
    fn migration_never_overwrites_the_new_files() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path());
        home.save_config(&Config {
            server: Some("https://b.example.com".into()),
            ..Default::default()
        })
        .unwrap();
        let mut creds = Credentials::default();
        creds.insert("https://a.example.com", "newer");
        home.save_credentials(&creds).unwrap();
        fs::write(home.legacy_path(), LEGACY).unwrap();

        home.migrate_legacy().unwrap();

        assert_eq!(
            home.load_config().unwrap().unwrap().server.as_deref(),
            Some("https://b.example.com")
        );
        assert_eq!(
            home.load_credentials()
                .unwrap()
                .unwrap()
                .token_for("https://a.example.com"),
            Some("newer")
        );
    }

    /// A legacy file this cannot read stays where it is, untouched.
    #[test]
    fn an_unreadable_legacy_file_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path());
        fs::write(home.legacy_path(), "{ nope").unwrap();
        assert!(home.migrate_legacy().is_err());
        assert_eq!(fs::read_to_string(home.legacy_path()).unwrap(), "{ nope");
        assert!(!home.credentials_path().exists());
    }
}
