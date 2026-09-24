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
//!     "unchecked":   ["recall.example.com\n1050\nt8Qm…=\n"],
//!     "unchecked_at": { "1050": "2026-10-02T09:14:05.402Z" },
//!     "unchecked_since": "2026-10-02T09:14:05.402Z",
//!     "last_proven_at": "2026-10-01T18:40:11.090Z" } } }
//! ```
//!
//! `checkpoints` are the ones a proof has shown the log extends, the newest
//! last. The newest implies every older one, since a log that extends it
//! extends everything it extended, so only the newest 32 are kept; the
//! older ones are for a person reading the file, not for the proof.
//!
//! `unchecked` are the ones seen since and not yet proven, and each of
//! those is evidence: the one saved just before a rewrite is the one that
//! shows it, whichever of them that turns out to be. So they are not
//! thinned. Up to 4096 are kept, a year and more of session starts on a
//! machine that never runs a check, and past that the ones dropped are
//! counted in `dropped`, which `recall doctor` fails on until
//! `recall audit reset`: a gap in the witnessing is said out loud, never
//! left to look like a clean record.
//!
//! `unchecked_at` is when each of those was saved, and `unchecked_since`
//! the oldest of them, which moves on as the oldest are proven.
//! `last_proven_at` is when a check last finished, and `unanswered` how
//! many checks in a row the server did not answer, counted only while
//! something is saved to be checked. While anything is saved, `recall
//! doctor` fails once either time is [`STALE_DAYS`] old or the count
//! reaches [`UNANSWERED_LIMIT`], so a server that keeps not answering, or
//! is too slow for the check ever to be asked, cannot keep one pending for
//! ever.
//!
//! Fields this build does not know are kept as they are, in the file, in an
//! entry and in a finding. One limit: a number in one of them beyond 64
//! bits comes back as the nearest `f64`, since `serde_json` reads it so
//! without its `arbitrary_precision` feature, and that feature would change
//! how every JSON number in the build is read, the offline verifier's
//! included. No field this format has holds one.
//!
//! # When it is checked
//!
//! Not in the hooks. A pull only saves the checkpoint it received, which
//! costs a read and a write of this file and no request; the check costs a
//! request per checkpoint to prove, and a session start is not the place to
//! spend them. The plan puts the check in `recall doctor`, so
//! [`Witness::check`] runs there and in `recall status`, which share one
//! collection, and in `recall audit verify` with no file: it asks the server
//! for its checkpoint now and for an RFC 9162 consistency proof from each
//! saved one to it, and verifies each with
//! [`recall_wire::audit::merkle::verify_consistency`]. The newest checked
//! one goes first and nothing is written down until it verifies, since a
//! server showing two forks shows each check one of them (see
//! [`Witness::check`]); after it the unchecked ones go smallest first, and
//! each is written down as checked the moment its proof verifies, so a
//! check cut short by the server's rate limit, a deadline or a lost
//! connection keeps what it proved, and the next one carries on from there.
//! `recall audit export` checks them too, without proofs, against the
//! leaves it fetched.
//!
//! What waiting costs is time: a rewrite is found at the next of those,
//! not at the pull after it. While more than [`NUDGE_AFTER`] wait, every
//! session's pull says so. Until then a checkpoint waits unproven, and
//! kept.
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
//! that has been rewritten once is not trusted again by default. A finding
//! that cannot be written down (the file's lock held too long, a disk
//! full) is still the answer, flagged as not saved, never an error in its
//! place. A backup restored on the server rolls its log back and is found
//! exactly like a rewrite, on purpose; the owner who did it knows why, and
//! resets.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use recall_wire::audit::merkle::{self, Hash, Tree};
use recall_wire::{AuditCheckpoint, AuditConsistencyResponse};
use serde_json::Value;

use crate::client::{self, Client};
use crate::home::{self, Error, FileLock};

/// `audit.json`, in `~/.recall`.
pub const AUDIT_FILE: &str = "audit.json";

/// Its lock, beside it.
const LOCK_FILE: &str = "audit.json.lock";

/// The format this build writes and reads: see [`home::VERSION`] for why a
/// file says which it is.
pub const FORMAT: u32 = 1;

/// How many checked checkpoints are kept: the newest stands for the rest.
const KEEP_CHECKED: usize = 32;

/// How many unchecked checkpoints are kept before any is dropped, and
/// counted: see the module docs.
const KEEP_UNCHECKED: usize = 4096;

/// How many may wait to be checked before every pull says so.
pub const NUDGE_AFTER: usize = 16;

/// How old, in days, the oldest unchecked checkpoint may grow, and the last
/// check that finished, before `recall doctor` fails on it rather than
/// warn.
pub const STALE_DAYS: i64 = 7;

/// How many checks in a row the server may leave unanswered (rate limited,
/// unreachable, too slow) before `recall doctor` fails on it rather than
/// warn.
pub const UNANSWERED_LIMIT: u64 = 10;

/// How long a hook waits for the lock before it gives up saving: the
/// checkpoint is then one fewer witnessed, and the pull is not held up.
const HOOK_WAIT: Duration = Duration::from_secs(2);

/// How long a command waits for it.
const COMMAND_WAIT: Duration = Duration::from_secs(20);

/// How long a check waits after the server's rate limit refuses a request
/// before asking again. The check's own deadline bounds how often.
const RETRY_WAIT: Duration = Duration::from_secs(5);

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

/// The file, as it is stored. Anything this build does not know, at either
/// level, is kept as it was read: a newer build's field survives an older
/// one's write.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct AuditFile {
    version: u32,
    #[serde(default)]
    servers: BTreeMap<String, Entry>,
    #[serde(flatten)]
    other: BTreeMap<String, Value>,
}

impl Default for AuditFile {
    fn default() -> Self {
        Self {
            version: FORMAT,
            servers: BTreeMap::new(),
            other: BTreeMap::new(),
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
    /// When each unchecked checkpoint was saved, by its size: sizes are
    /// unique among them, since a second root for one is a finding.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    unchecked_at: BTreeMap<u64, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unchecked_since: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_proven_at: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    dropped: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    unanswered: u64,
    /// Kept as it was read, so that what a newer build wrote into a
    /// finding survives this one's writes like any other field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    inconsistent: Option<Value>,
    #[serde(flatten)]
    other: BTreeMap<String, Value>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// What this machine holds for one server.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Saved {
    /// Checkpoints a proof has shown the log extends, smallest first.
    pub checkpoints: Vec<Checkpoint>,
    /// Checkpoints seen and not yet proven, smallest first.
    pub unchecked: Vec<Checkpoint>,
    /// When the oldest of [`Saved::unchecked`] was saved, in the API's
    /// timestamp format; [`None`] while none waits. It moves on as the
    /// oldest are proven.
    pub unchecked_since: Option<String>,
    /// When a check last finished, proving the log extends every
    /// checkpoint saved then; before the first, when the first checkpoint
    /// was proven. [`None`] while none has been.
    pub last_proven_at: Option<String>,
    /// How many unchecked checkpoints were dropped to keep within the
    /// bound, since the last reset: a gap in the witnessing.
    pub dropped: u64,
    /// How many checks in a row the server left unanswered, counted only
    /// while something is saved to be checked.
    pub unanswered: u64,
    /// What checking found, once it found the log does not extend one.
    pub inconsistent: Option<Inconsistency>,
    /// When each of [`Saved::unchecked`] was saved, by size.
    unchecked_at: BTreeMap<u64, String>,
    /// [`Saved::inconsistent`] as it was read, fields this build does not
    /// know included.
    inconsistent_as_read: Option<Value>,
    other: BTreeMap<String, Value>,
}

impl Saved {
    /// The newest proven checkpoint.
    pub fn newest(&self) -> Option<Checkpoint> {
        self.checkpoints.last().copied()
    }

    /// Whether nothing is held at all: nothing a reset would forget.
    pub fn is_empty(&self) -> bool {
        self.checkpoints.is_empty()
            && self.unchecked.is_empty()
            && self.inconsistent.is_none()
            && self.dropped == 0
            && self.unanswered == 0
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

    /// What a check has to prove, in the order it proves them: the newest
    /// checked checkpoint first, which stands for every older one, then
    /// each unchecked one, smallest first. See [`Witness::check`] for why
    /// that one goes first.
    fn to_prove(&self) -> Vec<Checkpoint> {
        let anchor = self.newest();
        anchor
            .into_iter()
            .chain(
                self.unchecked
                    .iter()
                    .copied()
                    .filter(|c| Some(*c) != anchor),
            )
            .collect()
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
        let inconsistent = match &entry.inconsistent {
            Some(raw) => Some(
                serde_json::from_value::<Inconsistency>(raw.clone()).map_err(|e| Error::Parse {
                    path: path.display().to_string(),
                    reason: format!("its finding for {origin} is not one: {e}"),
                })?,
            ),
            None => None,
        };
        Ok(Self {
            checkpoints: parse(&entry.checkpoints)?,
            unchecked: parse(&entry.unchecked)?,
            unchecked_since: entry.unchecked_since.clone(),
            last_proven_at: entry.last_proven_at.clone(),
            dropped: entry.dropped,
            unanswered: entry.unanswered,
            inconsistent,
            unchecked_at: entry.unchecked_at.clone(),
            inconsistent_as_read: entry.inconsistent.clone(),
            other: entry.other.clone(),
        })
    }

    /// The entry to store, within `keep` unchecked checkpoints: past that,
    /// the second smallest goes first (the smallest is the one most likely
    /// to predate a rewrite, the newest cover the most leaves), and each one
    /// that goes is counted.
    fn write(&self, origin: &str, keep: usize) -> Entry {
        let mut checkpoints = self.checkpoints.clone();
        checkpoints.sort();
        checkpoints.dedup();
        if checkpoints.len() > KEEP_CHECKED {
            checkpoints.drain(..checkpoints.len() - KEEP_CHECKED);
        }
        let mut unchecked = self.unchecked.clone();
        unchecked.sort();
        unchecked.dedup();
        let mut dropped = self.dropped;
        let keep = keep.max(2);
        if unchecked.len() > keep {
            let over = unchecked.len() - keep;
            unchecked.drain(1..1 + over);
            dropped += over as u64;
        }
        // Each still waiting keeps when it was saved. One without a time
        // was saved by a build that kept none, and gets the oldest time
        // known: never younger than it is.
        let unchecked_at: BTreeMap<u64, String> = unchecked
            .iter()
            .map(|c| {
                let at = self
                    .unchecked_at
                    .get(&c.size)
                    .or(self.unchecked_since.as_ref())
                    .cloned()
                    .unwrap_or_else(now);
                (c.size, at)
            })
            .collect();
        // The oldest of those, so that proving the oldest moves it on: one
        // saved a week ago and proven since no longer counts as waiting.
        let unchecked_since = unchecked_at.values().min().cloned();
        // Checked checkpoints with no time yet (a file from before it was
        // kept, or the first proven by a check still running) start the
        // clock now: see `recall doctor`, which fails once it is a week
        // old.
        let last_proven_at = match checkpoints.is_empty() {
            true => self.last_proven_at.clone(),
            false => self.last_proven_at.clone().or_else(|| Some(now())),
        };
        Entry {
            checkpoints: checkpoints.iter().map(|c| c.note(origin)).collect(),
            unchecked: unchecked.iter().map(|c| c.note(origin)).collect(),
            unchecked_at,
            unchecked_since,
            last_proven_at,
            dropped,
            unanswered: self.unanswered,
            inconsistent: self
                .inconsistent
                .as_ref()
                .map(|found| self.finding_as_stored(found)),
            other: self.other.clone(),
        }
    }

    /// A finding as it is stored: as it was read while it is the same
    /// finding, so that fields this build does not know are kept.
    fn finding_as_stored(&self, found: &Inconsistency) -> Value {
        match &self.inconsistent_as_read {
            Some(raw)
                if serde_json::from_value::<Inconsistency>(raw.clone())
                    .ok()
                    .as_ref()
                    == Some(found) =>
            {
                raw.clone()
            }
            _ => serde_json::json!({
                "found_at": found.found_at,
                "saved": found.saved,
                "seen": found.seen,
                "detail": found.detail,
            }),
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
    Inconsistent {
        /// What was found: the one written down earlier when there is one,
        /// since the first finding stands.
        finding: Inconsistency,
        /// Why it could not be written down, when it could not. The finding
        /// stands all the same; it is only not yet on disk.
        unsaved: Option<String>,
    },
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
    /// The check did not finish within its deadline. What it proved before
    /// then is kept.
    #[error("the server did not answer within {} seconds", .0.as_secs())]
    Deadline(Duration),
    /// Another check moved the newest checked checkpoint on while this one
    /// ran, having proven it against the log it was shown: what this one
    /// proved against its own does not follow from that, so it was not
    /// written down. Nothing about the log follows from this either.
    #[error("another check of the audit log finished while this one ran; run it again")]
    Moved,
}

impl CheckError {
    /// Whether the server did not answer at all: it could not be reached,
    /// asked to wait, or took longer than the deadline; or the request was
    /// never sent; or it refused this machine's credential
    /// ([`CheckError::refused`]). Nothing about its log follows from any
    /// of those, which is why they warn rather than fail, until
    /// [`UNANSWERED_LIMIT`] of them in a row. Anything else the server sent
    /// in place of a proof (another status, a body that is not one, a
    /// redirect) is an answer, and not a proof: `recall doctor` fails on
    /// it.
    pub fn unanswered(&self) -> bool {
        if self.refused() {
            return true;
        }
        match self {
            CheckError::Server(client::Error::Status { code, .. }) => *code == 429,
            CheckError::Server(
                client::Error::Transport(_)
                | client::Error::Sign(_)
                | client::Error::Nonce(_)
                | client::Error::Invalid(_),
            ) => true,
            CheckError::Deadline(_) => true,
            _ => false,
        }
    }

    /// Whether the server refused this machine's credential (401 or 403): a
    /// device revoked or swept, or one the audit routes are not open to.
    /// That says something about the credential and nothing about the log,
    /// so it counts as unanswered, and is reported with what to do about
    /// the credential rather than as a server that would not prove.
    pub fn refused(&self) -> bool {
        matches!(
            self,
            CheckError::Server(client::Error::Status {
                code: 401 | 403,
                ..
            })
        )
    }

    /// Whether the server keeps no audit log at all: one older than 0.4.2,
    /// which answers the audit routes 404.
    pub fn no_log(&self) -> bool {
        matches!(
            self,
            CheckError::Server(client::Error::Status { code: 404, .. })
        )
    }

    /// Whether `audit.json` itself could not be read: it may hold the only
    /// record of a rewrite, so that is never taken lightly.
    pub fn unreadable(&self) -> bool {
        matches!(
            self,
            CheckError::File(Error::Read { .. } | Error::Parse { .. })
        )
    }
}

/// This machine's record of one server's audit log, in one `audit.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Witness {
    file: PathBuf,
    key: String,
    origin: String,
    /// How many unchecked checkpoints are kept: [`KEEP_UNCHECKED`], less in
    /// tests.
    keep: usize,
    /// How long a command waits for the file's lock: [`COMMAND_WAIT`], less
    /// in tests.
    wait: Duration,
}

impl Witness {
    /// The record of the server at `url`, kept in `file`.
    pub fn new(file: impl Into<PathBuf>, url: &str) -> Self {
        Self {
            file: file.into(),
            key: home::normalize_url(url),
            origin: origin(url),
            keep: KEEP_UNCHECKED,
            wait: COMMAND_WAIT,
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
                    saved.unchecked_at.insert(seen.size, now());
                    true
                }
            }
        })
        .map(|_| ())
    }

    /// Asks the server whether its log still extends every checkpoint saved
    /// here: its checkpoint now, and a consistency proof from each saved one
    /// that needs one. On an inconsistency, that is written down and nothing
    /// else changes. A request the server's rate limit refuses is asked
    /// again after a pause, and the whole check stops at `deadline`,
    /// keeping what it proved.
    ///
    /// The newest checked checkpoint is proven first, and nothing is
    /// written down until it is. It stands for every older one, but only
    /// for the log it was proven against: a server showing this machine
    /// two forks shows each check one of them, and a checkpoint proven
    /// against this check's fork says nothing about the fork the newest
    /// checked one came from. Marked checked before that one is proven
    /// against the same log, it would join the two histories into one
    /// record that every later check passes. Once it is, each unchecked
    /// checkpoint, smallest first, moves to the checked ones as soon as its
    /// proof verifies, so a check cut short keeps what it proved; and the
    /// server's checkpoint now becomes the newest once all have.
    ///
    /// Each of those writes happens only while the newest checked
    /// checkpoint in the file is still one this check proved: another check
    /// finishing meanwhile proved its own against the log it was shown, and
    /// then nothing is written and the answer is [`CheckError::Moved`].
    ///
    /// With nothing saved yet, the server's checkpoint now is saved as the
    /// first, taken on trust: every log has to be first seen some time.
    ///
    /// How many checks in a row went unanswered is kept while anything is
    /// saved to be checked, and reset by any answer: see
    /// [`UNANSWERED_LIMIT`].
    pub async fn check(
        &self,
        client: &Client,
        deadline: Duration,
    ) -> Result<Witnessed, CheckError> {
        let outcome = match tokio::time::timeout(deadline, self.check_now(client)).await {
            Ok(outcome) => outcome,
            Err(_) => Err(CheckError::Deadline(deadline)),
        };
        let unanswered = match &outcome {
            Err(e) if e.unanswered() => Some(true),
            Err(CheckError::File(_) | CheckError::Moved) => None,
            _ => Some(false),
        };
        if let Some(unanswered) = unanswered {
            // Best effort: the outcome is the answer whether or not the
            // count could be written.
            let _ = self.update(self.wait, |held| {
                let before = held.unanswered;
                // A check with nothing to check left nothing pending.
                let pending = !held.all().is_empty();
                held.unanswered = match unanswered && pending {
                    true => before + 1,
                    false => 0,
                };
                held.unanswered != before
            });
        }
        outcome
    }

    async fn check_now(&self, client: &Client) -> Result<Witnessed, CheckError> {
        let saved = self.load()?;
        if let Some(finding) = saved.inconsistent {
            return Ok(Witnessed::Inconsistent {
                finding,
                unsaved: None,
            });
        }
        let answer = patiently(|| client.audit_checkpoint()).await?;
        let current = Checkpoint::from_wire(&answer).ok_or_else(|| {
            CheckError::Malformed(format!(
                "the server's checkpoint is not a size and a 32-byte root: {}",
                answer.to_header_value()
            ))
        })?;
        let anchor = saved.newest();
        // Every checkpoint shown to be a prefix of `current`, which is one
        // of itself: any two of them are prefixes of one log.
        let mut proven = vec![current];
        let to_prove = saved.to_prove();
        // What needs no proof is looked at before any proof is asked for:
        // a checkpoint longer than the log now, or of its size with another
        // root, is a finding whatever the proofs would say, and must not
        // wait behind one the server stalls. It writes nothing but the
        // finding, so the anchor still goes first for everything written.
        for &held in &to_prove {
            if let Compared::Fails(detail) = compare(held, current) {
                return Ok(self.found(held, current, detail));
            }
        }
        for held in to_prove {
            let holds = match compare(held, current) {
                Compared::Holds => Ok(()),
                Compared::Fails(detail) => Err(detail),
                Compared::NeedsProof => {
                    let proof =
                        patiently(|| client.audit_consistency(held.size, current.size)).await?;
                    prove(held, current, &proof)?
                }
            };
            if let Err(detail) = holds {
                return Ok(self.found(held, current, detail));
            }
            proven.push(held);
            // Kept now, not at the end, so a check cut short keeps it; the
            // anchor itself is checked already.
            if Some(held) != anchor && !self.commit(&proven, &[held], None)? {
                return self.not_committed();
            }
        }
        if !self.commit(&proven, &[], Some(current))? {
            return self.not_committed();
        }
        Ok(Witnessed::Extends {
            current,
            proved: proven.len() - 1,
        })
    }

    /// Why a commit wrote nothing: a finding written meanwhile, which
    /// stands, or another check moving the newest checked checkpoint on.
    fn not_committed(&self) -> Result<Witnessed, CheckError> {
        match self.load()?.inconsistent {
            Some(finding) => Ok(Witnessed::Inconsistent {
                finding,
                unsaved: None,
            }),
            None => Err(CheckError::Moved),
        }
    }

    /// [`Witness::check`] for `recall audit export`, which holds every leaf
    /// up to `current`: each saved checkpoint is checked against the tree
    /// they make, with no proof to ask for. An error only when the file
    /// could not be read, or what checked out could not be written down.
    pub fn witness_export(&self, tree: &Tree, current: Checkpoint) -> Result<Witnessed, Error> {
        let saved = self.load()?;
        if let Some(finding) = saved.inconsistent {
            return Ok(Witnessed::Inconsistent {
                finding,
                unsaved: None,
            });
        }
        // Checked before the lock is taken, so that a finding is the answer
        // even when the lock cannot be had.
        if let Some((held, detail)) = off_tree(&saved, tree, current) {
            return Ok(self.found(held, current, detail));
        }
        // And again under the lock, over what the file holds by then, which
        // is what is promoted: a checkpoint another check promoted
        // meanwhile is on this tree too, or it is a finding, never taken on
        // trust.
        let mut late = None;
        let mut proved = 0;
        self.update(self.wait, |held| {
            if held.inconsistent.is_some() {
                return false;
            }
            if let Some(off) = off_tree(held, tree, current) {
                late = Some(off);
                return false;
            }
            let all = held.all();
            proved = all.len();
            held.unchecked.clear();
            held.checkpoints = all;
            if current.size > 0 {
                held.checkpoints.push(current);
                held.last_proven_at = Some(now());
            }
            true
        })?;
        if let Some((held, detail)) = late {
            return Ok(self.found(held, current, detail));
        }
        if let Some(finding) = self.load()?.inconsistent {
            return Ok(Witnessed::Inconsistent {
                finding,
                unsaved: None,
            });
        }
        Ok(Witnessed::Extends { current, proved })
    }

    /// Forgets everything held for this server: its checkpoints, the count
    /// of any dropped, and any inconsistency found in them. Answers what was
    /// held.
    pub fn reset(&self) -> Result<Saved, Error> {
        let _lock = FileLock::take(self.dir(), LOCK_FILE, self.wait)?;
        let mut file = self.read_file()?;
        let held = match file.servers.remove(&self.key) {
            Some(entry) => Saved::read(&entry, &self.origin, &self.file)?,
            None => return Ok(Saved::default()),
        };
        self.write_file(&file)?;
        Ok(held)
    }

    /// Writes an inconsistency down, unless one already is, and answers
    /// the one that stands: the first found. When it cannot be written, the
    /// answer is still that inconsistency, flagged as not saved.
    fn found(&self, saved: Checkpoint, seen: Checkpoint, detail: String) -> Witnessed {
        let fresh = self.inconsistency(saved, seen, detail);
        let mut standing = None;
        let written = self.update(self.wait, |held| {
            let first = held.inconsistent.is_none();
            if first {
                held.inconsistent = Some(fresh.clone());
            }
            standing = held.inconsistent.clone();
            first
        });
        match written {
            Ok(_) => Witnessed::Inconsistent {
                finding: standing.unwrap_or(fresh),
                unsaved: None,
            },
            Err(e) => Witnessed::Inconsistent {
                finding: fresh,
                unsaved: Some(e.to_string()),
            },
        }
    }

    /// Moves `promote` to the checked checkpoints, and `current`, when
    /// given, joins them as the newest, the check having finished. All of
    /// them are among `proven`: prefixes of the one log this check was
    /// shown, and so of each other.
    ///
    /// Read afresh under the lock, and written only while the newest
    /// checked checkpoint there is one of `proven` too (or there is none),
    /// so that every checked checkpoint stays a prefix of the newest: one
    /// another check promoted meanwhile was proven against the log that
    /// check was shown, which may be another fork. A checkpoint a hook
    /// saved meanwhile stays unchecked rather than being lost, and an
    /// inconsistency found meanwhile stays found: nothing is marked checked
    /// beside it. Answers whether it wrote.
    fn commit(
        &self,
        proven: &[Checkpoint],
        promote: &[Checkpoint],
        current: Option<Checkpoint>,
    ) -> Result<bool, Error> {
        let mut committed = false;
        self.update(self.wait, |held| {
            if held.inconsistent.is_some() {
                return false;
            }
            if held
                .newest()
                .is_some_and(|newest| !proven.contains(&newest))
            {
                return false;
            }
            held.unchecked.retain(|c| !promote.contains(c));
            held.checkpoints.extend_from_slice(promote);
            if let Some(current) = current.filter(|c| c.size > 0) {
                held.checkpoints.push(current);
                held.last_proven_at = Some(now());
            }
            committed = true;
            true
        })?;
        Ok(committed)
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
            .insert(self.key.clone(), held.write(&self.origin, self.keep));
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

/// A request asked again, after a pause, while the server's rate limit
/// refuses it. Only a deadline around it stops it: the server's window is
/// a minute, and a check that gives up first proves nothing more.
async fn patiently<T, F, Fut>(mut call: F) -> Result<T, client::Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, client::Error>>,
{
    loop {
        match call().await {
            Err(client::Error::Status { code: 429, .. }) => tokio::time::sleep(RETRY_WAIT).await,
            other => return other,
        }
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

/// The first checkpoint `saved` holds that is not a prefix of `tree`, the
/// log at `current`, and why; [`None`] when every one is.
fn off_tree(saved: &Saved, tree: &Tree, current: Checkpoint) -> Option<(Checkpoint, String)> {
    saved.all().into_iter().find_map(|held| {
        if held.size > tree.size() {
            return Some((held, shorter(held, current)));
        }
        let root = tree.root_at(held.size);
        (root != held.root).then(|| {
            let detail = format!(
                "the root over the log's first {} leaves is {}, not the {} saved here: history \
                 before it was rewritten",
                held.size,
                STANDARD.encode(root),
                STANDARD.encode(held.root)
            );
            (held, detail)
        })
    })
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
