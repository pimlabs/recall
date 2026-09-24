//! `~/.recall/audit.json`: this machine as a witness of each server's audit
//! log.
//!
//! A Merkle tree proves something only to someone holding an earlier root
//! (`docs/design/part5-plan.md`, "Who witnesses"). Every `GET /sync` answer
//! carries one, the `Recall-Audit-Checkpoint` header, and every response
//! that carries it is saved here by [`Witness::record`], which the
//! [`Client`] calls itself, so a pull saves one whoever made it: the
//! session-start hook, `recall status`, `recall promote`.
//!
//! # What is kept
//!
//! Per server, keyed by its URL as `credentials.toml` keys it, each
//! checkpoint as the three lines of a C2SP tlog-checkpoint note: the
//! server's address as the origin (the URL without its scheme), the tree
//! size, the root in standard base64, and no signature. The owner's
//! devices fetch a checkpoint over TLS from the very server they are
//! checking, so a signature by that server would add nothing for one
//! owner; one can be added to the note if a third-party witness is ever
//! wanted.
//!
//! ```json
//! { "version": 1, "servers": { "https://recall.example.com": {
//!     "checkpoints": ["recall.example.com\n1042\nCsUY…=\n"],
//!     "unchecked":   ["recall.example.com\n1050\nt8Qm…=\n"] } } }
//! ```
//!
//! `checkpoints` are the ones a proof has shown the log extends, the newest
//! last; `unchecked`, the ones seen since and not yet proven. The newest
//! checked one implies all the older ones, since the log that extends it
//! extends everything it extended, so only 32 of each are kept: the older
//! checked ones are for a person reading the file, not for the proof.
//!
//! # When it is checked
//!
//! Not in the hooks. A pull only saves the checkpoint it received, which
//! costs a read and a write of this small file and no request; the check
//! costs a request per checkpoint to prove, and a session start is not the
//! place to spend them. The plan puts the check in `recall doctor`, so
//! [`Witness::check`] runs there and in `recall status`, which share one
//! collection, and in `recall audit verify` with no file: it asks the server
//! for its checkpoint now and for an RFC 9162 consistency proof from each
//! saved one to it, and verifies each with
//! [`recall_wire::audit::merkle::verify_consistency`]. `recall audit export`
//! checks them too, without proofs, against the leaves it fetched.
//!
//! What waiting costs is time: a rewrite is found at the next of those,
//! not at the pull after it. Nothing is lost by waiting, because nothing
//! is taken on trust in the meantime: a checkpoint stays unchecked, and so
//! kept, until a proof covers it. When more than 32 wait, the smallest is
//! kept (the one most likely to predate a rewrite) with the 31 newest.
//!
//! One thing is found at once, by the pull itself: a checkpoint at a size
//! already saved with another root. Two roots for one size need no proof
//! to show a fork.
//!
//! # What an inconsistency does
//!
//! It is written into the server's entry as `inconsistent`, beside every
//! checkpoint that was kept, and nothing clears it but `recall audit reset`
//! ([`Witness::reset`]). Until then `recall doctor` fails on it, `recall
//! status` shows it, the pull hook says so at every session start (still
//! exiting 0), and no later checkpoint is taken as the new truth: a log
//! that has been rewritten once is not trusted again by default. A backup
//! restored on the server rolls its log back and is found exactly like a
//! rewrite, on purpose; the owner who did it knows why, and resets.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use recall_wire::audit::merkle::{self, Hash, Tree};
use recall_wire::{AuditCheckpoint, AuditConsistencyResponse};

use crate::client::{self, Client};
use crate::home::{self, Error, FileLock};

/// `audit.json`, in `~/.recall`.
pub const AUDIT_FILE: &str = "audit.json";

/// Its lock, beside it.
const LOCK_FILE: &str = "audit.json.lock";

/// The format this build writes and reads: see [`home::VERSION`] for why a
/// file says which it is.
pub const FORMAT: u32 = 1;

/// How many checkpoints of each kind are kept: see the module docs.
const KEEP: usize = 32;

/// How long a hook waits for the lock before it gives up saving: the
/// checkpoint is then one fewer witnessed, and the pull is not held up.
const HOOK_WAIT: Duration = Duration::from_secs(2);

/// How long a command waits for it.
const COMMAND_WAIT: Duration = Duration::from_secs(20);

/// One checkpoint of a server's audit log: how many leaves, and their root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Checkpoint {
    /// The tree's size.
    pub size: u64,
    /// Its root.
    pub root: Hash,
}

impl Checkpoint {
    /// From a `Recall-Audit-Checkpoint` header's value, `<size> <root>`.
    /// [`None`] unless the root is 32 bytes in canonical standard base64.
    pub fn from_header(value: &str) -> Option<Self> {
        Self::from_wire(&AuditCheckpoint::parse_header_value(value)?)
    }

    /// From `GET /v1/audit/checkpoint`'s answer, likewise.
    pub fn from_wire(cp: &AuditCheckpoint) -> Option<Self> {
        Some(Self {
            size: cp.tree_size,
            root: cp.root()?,
        })
    }

    /// `<size> <root>`: the header's form, and an export's first line.
    pub fn header(&self) -> String {
        format!("{} {}", self.size, STANDARD.encode(self.root))
    }

    /// The three lines of a C2SP tlog-checkpoint note with no signature:
    /// `origin`, the size, the root, each ending in a newline.
    pub fn note(&self, origin: &str) -> String {
        format!("{origin}\n{}\n{}\n", self.size, STANDARD.encode(self.root))
    }

    /// Reads [`Checkpoint::note`] back, for `origin` only.
    fn from_note(note: &str, origin: &str) -> Option<Self> {
        let body = note.strip_suffix('\n')?;
        let mut lines = body.split('\n');
        let (o, size, root) = (lines.next()?, lines.next()?, lines.next()?);
        if o != origin || lines.next().is_some() {
            return None;
        }
        Some(Self {
            // One spelling of each number, as for the root.
            size: size.parse().ok().filter(|n: &u64| n.to_string() == size)?,
            root: recall_wire::audit::verify::root_hash(root)?,
        })
    }
}

/// A server's address as a checkpoint note's origin: its URL, spelled as
/// [`home::normalize_url`] spells it, without the scheme. C2SP asks for a
/// schema-less URL.
pub fn origin(url: &str) -> String {
    let url = home::normalize_url(url);
    match url.split_once("://") {
        Some((_, rest)) => rest.to_string(),
        None => url,
    }
}

/// What checking found out and wrote down: the log no longer extends a
/// checkpoint saved here.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Inconsistency {
    /// When this machine found it, in the API's timestamp format.
    pub found_at: String,
    /// The checkpoint saved earlier that the log does not extend, as its
    /// note.
    pub saved: String,
    /// The log as the server showed it then, as a note.
    pub seen: String,
    /// What did not hold, in a sentence.
    pub detail: String,
}

impl Inconsistency {
    /// [`Inconsistency::saved`] as `<size> <root>`, for a person to read.
    pub fn saved_header(&self) -> String {
        note_as_header(&self.saved)
    }

    /// [`Inconsistency::seen`] likewise.
    pub fn seen_header(&self) -> String {
        note_as_header(&self.seen)
    }
}

/// A note without its origin line, its size and root on one line.
fn note_as_header(note: &str) -> String {
    note.lines().skip(1).collect::<Vec<_>>().join(" ")
}

/// The file, as it is stored.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct AuditFile {
    version: u32,
    #[serde(default)]
    servers: BTreeMap<String, Entry>,
}

impl Default for AuditFile {
    fn default() -> Self {
        Self {
            version: FORMAT,
            servers: BTreeMap::new(),
        }
    }
}

/// One server's entry, as it is stored: notes, not parsed.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Entry {
    #[serde(default)]
    checkpoints: Vec<String>,
    #[serde(default)]
    unchecked: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    inconsistent: Option<Inconsistency>,
}

/// What this machine holds for one server.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Saved {
    /// Checkpoints a proof has shown the log extends, smallest first.
    pub checkpoints: Vec<Checkpoint>,
    /// Checkpoints seen and not yet proven, smallest first.
    pub unchecked: Vec<Checkpoint>,
    /// What checking found, once it found the log does not extend one.
    pub inconsistent: Option<Inconsistency>,
}

impl Saved {
    /// The newest proven checkpoint.
    pub fn newest(&self) -> Option<Checkpoint> {
        self.checkpoints.last().copied()
    }

    /// Whether nothing is held at all.
    pub fn is_empty(&self) -> bool {
        self.checkpoints.is_empty() && self.unchecked.is_empty() && self.inconsistent.is_none()
    }

    /// Every checkpoint held, checked or not, smallest first: what an
    /// export must still extend.
    pub fn all(&self) -> Vec<Checkpoint> {
        let mut all: Vec<Checkpoint> = self
            .checkpoints
            .iter()
            .chain(&self.unchecked)
            .copied()
            .collect();
        all.sort();
        all.dedup();
        all
    }

    /// What a check has to prove: the newest checked checkpoint, which
    /// stands for every older one, and each unchecked one.
    fn to_prove(&self) -> Vec<Checkpoint> {
        let mut out: Vec<Checkpoint> = self
            .newest()
            .into_iter()
            .chain(self.unchecked.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    fn read(entry: &Entry, origin: &str, path: &Path) -> Result<Self, Error> {
        let parse = |notes: &[String]| -> Result<Vec<Checkpoint>, Error> {
            let mut out = notes
                .iter()
                .map(|n| {
                    Checkpoint::from_note(n, origin).ok_or_else(|| Error::Parse {
                        path: path.display().to_string(),
                        reason: format!("{n:?} is not a checkpoint of {origin}"),
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            out.sort();
            out.dedup();
            Ok(out)
        };
        Ok(Self {
            checkpoints: parse(&entry.checkpoints)?,
            unchecked: parse(&entry.unchecked)?,
            inconsistent: entry.inconsistent.clone(),
        })
    }

    fn write(&self, origin: &str) -> Entry {
        let mut checkpoints = self.checkpoints.clone();
        checkpoints.sort();
        checkpoints.dedup();
        if checkpoints.len() > KEEP {
            checkpoints.drain(..checkpoints.len() - KEEP);
        }
        let mut unchecked = self.unchecked.clone();
        unchecked.sort();
        unchecked.dedup();
        if unchecked.len() > KEEP {
            // The smallest, and the newest after it.
            unchecked.drain(1..unchecked.len() - (KEEP - 1));
        }
        Entry {
            checkpoints: checkpoints.iter().map(|c| c.note(origin)).collect(),
            unchecked: unchecked.iter().map(|c| c.note(origin)).collect(),
            inconsistent: self.inconsistent.clone(),
        }
    }
}

/// What [`Witness::check`] and [`Witness::witness_export`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Witnessed {
    /// The log extends every checkpoint saved here.
    Extends {
        /// The log as it is now, saved as the newest checkpoint.
        current: Checkpoint,
        /// How many saved checkpoints this proved, the newest checked one
        /// among them.
        proved: usize,
    },
    /// It does not: recorded, and reported until `recall audit reset`.
    Inconsistent(Inconsistency),
}

/// Why [`Witness::check`] could not say either way.
#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    /// `audit.json` could not be read or written.
    #[error(transparent)]
    File(#[from] Error),
    /// The server could not be asked, or said no.
    #[error("{}", .0.reason())]
    Server(#[from] client::Error),
    /// The server answered with something that is not a checkpoint or a
    /// proof for what was asked.
    #[error("{0}")]
    Malformed(String),
}

impl CheckError {
    /// Whether the server was not asked at all, or asked to wait: nothing
    /// about its log follows from that.
    pub fn unanswered(&self) -> bool {
        match self {
            CheckError::Server(client::Error::Status { code, .. }) => *code == 429,
            CheckError::Server(_) => true,
            _ => false,
        }
    }

    /// Whether the server keeps no audit log at all: one older than 0.4.2,
    /// which answers the audit routes 404.
    pub fn no_log(&self) -> bool {
        matches!(
            self,
            CheckError::Server(client::Error::Status { code: 404, .. })
        )
    }
}

/// This machine's record of one server's audit log, in one `audit.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Witness {
    file: PathBuf,
    key: String,
    origin: String,
}

impl Witness {
    /// The record of the server at `url`, kept in `file`.
    pub fn new(file: impl Into<PathBuf>, url: &str) -> Self {
        Self {
            file: file.into(),
            key: home::normalize_url(url),
            origin: origin(url),
        }
    }

    /// The file.
    pub fn file(&self) -> &Path {
        &self.file
    }

    /// The server's address, as its notes name it.
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// What is held for this server now; empty when nothing is.
    pub fn load(&self) -> Result<Saved, Error> {
        let file = self.read_file()?;
        match file.servers.get(&self.key) {
            Some(entry) => Saved::read(entry, &self.origin, &self.file),
            None => Ok(Saved::default()),
        }
    }

    /// Saves a checkpoint a response carried, as a `Recall-Audit-Checkpoint`
    /// header value. What the pull hook runs, so it waits for the file's
    /// lock only briefly, and a value that is not a checkpoint is ignored:
    /// see the module docs for why a hook checks nothing more here than a
    /// second root for a size already saved.
    pub fn record(&self, header: &str) -> Result<(), Error> {
        let Some(seen) = Checkpoint::from_header(header) else {
            return Ok(());
        };
        if seen.size == 0 {
            // An empty log: nothing to witness.
            return Ok(());
        }
        self.update(HOOK_WAIT, |saved| {
            let same_size = saved
                .checkpoints
                .iter()
                .chain(&saved.unchecked)
                .find(|c| c.size == seen.size)
                .copied();
            match same_size {
                Some(known) if known.root == seen.root => false,
                Some(known) => {
                    if saved.inconsistent.is_none() {
                        saved.inconsistent = Some(self.inconsistency(
                            known,
                            seen,
                            format!(
                                "the log answered with a second root for size {}: it was \
                                 rewritten, or is not the same log for everyone",
                                seen.size
                            ),
                        ));
                    }
                    true
                }
                None => {
                    saved.unchecked.push(seen);
                    true
                }
            }
        })
        .map(|_| ())
    }

    /// Asks the server whether its log still extends every checkpoint saved
    /// here: its checkpoint now, and a consistency proof from each saved one
    /// that needs one. On success, what was proven moves to the checked
    /// ones and the server's checkpoint now becomes the newest; on an
    /// inconsistency, that is written down and nothing else changes.
    ///
    /// With nothing saved yet, the server's checkpoint now is saved as the
    /// first, taken on trust: every log has to be first seen some time.
    pub async fn check(&self, client: &Client) -> Result<Witnessed, CheckError> {
        let saved = self.load()?;
        if let Some(found) = saved.inconsistent {
            return Ok(Witnessed::Inconsistent(found));
        }
        let answer = client.audit_checkpoint().await?;
        let current = Checkpoint::from_wire(&answer).ok_or_else(|| {
            CheckError::Malformed(format!(
                "the server's checkpoint is not a size and a 32-byte root: {}",
                answer.to_header_value()
            ))
        })?;
        let mut proved = Vec::new();
        for held in saved.to_prove() {
            let holds = match compare(held, current) {
                Compared::Holds => Ok(()),
                Compared::Fails(detail) => Err(detail),
                Compared::NeedsProof => {
                    let proof = client.audit_consistency(held.size, current.size).await?;
                    prove(held, current, &proof)?
                }
            };
            match holds {
                Ok(()) => proved.push(held),
                Err(detail) => {
                    return Ok(Witnessed::Inconsistent(self.found(
                        held,
                        current,
                        detail,
                        COMMAND_WAIT,
                    )?))
                }
            }
        }
        self.commit(&proved, current)?;
        Ok(Witnessed::Extends {
            current,
            proved: proved.len(),
        })
    }

    /// [`Witness::check`] for `recall audit export`, which holds every leaf
    /// up to `current`: each saved checkpoint is checked against the tree
    /// they make, with no proof to ask for.
    pub fn witness_export(&self, tree: &Tree, current: Checkpoint) -> Result<Witnessed, Error> {
        let saved = self.load()?;
        if let Some(found) = saved.inconsistent {
            return Ok(Witnessed::Inconsistent(found));
        }
        let mut proved = Vec::new();
        for held in saved.all() {
            if held.size > tree.size() {
                let detail = shorter(held, current);
                return Ok(Witnessed::Inconsistent(self.found(
                    held,
                    current,
                    detail,
                    COMMAND_WAIT,
                )?));
            }
            if tree.root_at(held.size) != held.root {
                let detail = format!(
                    "the root over the log's first {} leaves is {}, not the {} saved here: \
                     history before it was rewritten",
                    held.size,
                    STANDARD.encode(tree.root_at(held.size)),
                    STANDARD.encode(held.root)
                );
                return Ok(Witnessed::Inconsistent(self.found(
                    held,
                    current,
                    detail,
                    COMMAND_WAIT,
                )?));
            }
            proved.push(held);
        }
        self.commit(&proved, current)?;
        Ok(Witnessed::Extends {
            current,
            proved: proved.len(),
        })
    }

    /// Forgets everything held for this server: its checkpoints and any
    /// inconsistency found in them. Answers what was held.
    pub fn reset(&self) -> Result<Saved, Error> {
        let _lock = FileLock::take(self.dir(), LOCK_FILE, COMMAND_WAIT)?;
        let mut file = self.read_file()?;
        let held = match file.servers.remove(&self.key) {
            Some(entry) => Saved::read(&entry, &self.origin, &self.file)?,
            None => return Ok(Saved::default()),
        };
        self.write_file(&file)?;
        Ok(held)
    }

    /// Writes an inconsistency down, unless one already is, and answers
    /// the one that stands.
    fn found(
        &self,
        saved: Checkpoint,
        seen: Checkpoint,
        detail: String,
        wait: Duration,
    ) -> Result<Inconsistency, Error> {
        let mut standing = None;
        self.update(wait, |held| {
            if held.inconsistent.is_none() {
                held.inconsistent = Some(self.inconsistency(saved, seen, detail));
            }
            standing = held.inconsistent.clone();
            true
        })?;
        Ok(standing.expect("set above"))
    }

    /// `proved` are prefixes of the log at `current`: they move to the
    /// checked ones, and `current` joins them as the newest. Read afresh
    /// under the lock, so a checkpoint a hook saved meanwhile stays
    /// unchecked rather than being lost, and an inconsistency found
    /// meanwhile stays found.
    fn commit(&self, proved: &[Checkpoint], current: Checkpoint) -> Result<(), Error> {
        self.update(COMMAND_WAIT, |held| {
            if held.inconsistent.is_some() {
                return false;
            }
            held.unchecked.retain(|c| !proved.contains(c));
            held.checkpoints.extend_from_slice(proved);
            if current.size > 0 {
                held.checkpoints.push(current);
            }
            true
        })
        .map(|_| ())
    }

    fn inconsistency(&self, saved: Checkpoint, seen: Checkpoint, detail: String) -> Inconsistency {
        Inconsistency {
            found_at: now(),
            saved: saved.note(&self.origin),
            seen: seen.note(&self.origin),
            detail,
        }
    }

    /// Reads, changes and writes this server's entry under the file's lock.
    /// `change` answers whether it changed anything; nothing is written
    /// when it did not. A file that cannot be read is never written over:
    /// it may hold the only record of an inconsistency.
    fn update(
        &self,
        wait: Duration,
        change: impl FnOnce(&mut Saved) -> bool,
    ) -> Result<bool, Error> {
        let _lock = FileLock::take(self.dir(), LOCK_FILE, wait)?;
        let mut file = self.read_file()?;
        let mut held = match file.servers.get(&self.key) {
            Some(entry) => Saved::read(entry, &self.origin, &self.file)?,
            None => Saved::default(),
        };
        if !change(&mut held) {
            return Ok(false);
        }
        file.servers
            .insert(self.key.clone(), held.write(&self.origin));
        self.write_file(&file)?;
        Ok(true)
    }

    fn dir(&self) -> &Path {
        self.file.parent().unwrap_or_else(|| Path::new("."))
    }

    fn read_file(&self) -> Result<AuditFile, Error> {
        let path = self.file.display().to_string();
        let bytes = match std::fs::read(&self.file) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(AuditFile::default()),
            Err(source) => return Err(Error::Read { path, source }),
        };
        let file: AuditFile = serde_json::from_slice(&bytes).map_err(|e| Error::Parse {
            path: path.clone(),
            reason: e.to_string(),
        })?;
        if file.version != FORMAT {
            return Err(Error::Parse {
                path,
                reason: format!(
                    "version {} (this build understands {FORMAT}), nothing was changed",
                    file.version
                ),
            });
        }
        Ok(file)
    }

    fn write_file(&self, file: &AuditFile) -> Result<(), Error> {
        let err = |source| Error::Write {
            path: self.file.display().to_string(),
            source,
        };
        let mut body =
            serde_json::to_vec_pretty(file).map_err(|e| err(std::io::Error::other(e)))?;
        body.push(b'\n');
        crate::atomic::write(&self.file, ".recall-", ".tmp", &body).map_err(err)
    }
}

/// What can be said of a saved checkpoint against the log now without a
/// proof.
#[derive(Debug, PartialEq, Eq)]
enum Compared {
    Holds,
    Fails(String),
    NeedsProof,
}

fn compare(held: Checkpoint, current: Checkpoint) -> Compared {
    if held.size == 0 {
        return if held.root == merkle::empty_root() {
            Compared::Holds
        } else {
            Compared::Fails(
                "a checkpoint of an empty log with a root an empty log does not have".into(),
            )
        };
    }
    if held.size > current.size {
        return Compared::Fails(shorter(held, current));
    }
    if held.size == current.size {
        return if held.root == current.root {
            Compared::Holds
        } else {
            Compared::Fails(format!(
                "the log has {} leaves, as it had when this checkpoint was saved, under another \
                 root: history was rewritten",
                current.size
            ))
        };
    }
    Compared::NeedsProof
}

fn shorter(held: Checkpoint, current: Checkpoint) -> String {
    format!(
        "the log has {} leaves, fewer than the {} of a checkpoint saved here: it was rolled \
         back or rewritten",
        current.size, held.size
    )
}

/// Whether `proof` shows the log at `current` extends `held`: `Ok(Ok(()))`
/// when it does, `Ok(Err(why))` when it does not, and an error when it is
/// not a proof for these two sizes at all.
fn prove(
    held: Checkpoint,
    current: Checkpoint,
    proof: &AuditConsistencyResponse,
) -> Result<Result<(), String>, CheckError> {
    if (proof.first, proof.second) != (held.size, current.size) {
        return Err(CheckError::Malformed(format!(
            "asked for a proof from {} to {}, the server answered one from {} to {}",
            held.size, current.size, proof.first, proof.second
        )));
    }
    let nodes = proof
        .proof
        .iter()
        .map(|n| recall_wire::audit::verify::root_hash(n))
        .collect::<Option<Vec<Hash>>>()
        .ok_or_else(|| CheckError::Malformed("the proof holds a node that is not a hash".into()))?;
    if merkle::verify_consistency(held.size, &held.root, current.size, &current.root, &nodes) {
        Ok(Ok(()))
    } else {
        Ok(Err(format!(
            "the server's proof that its log of {} leaves extends the {} saved here does not \
             verify: history before it was rewritten",
            current.size, held.size
        )))
    }
}

/// Now, in the API's timestamp format.
fn now() -> String {
    let fmt = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    time::OffsetDateTime::now_utc()
        .format(&fmt)
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "audit_tests.rs"]
mod tests;
