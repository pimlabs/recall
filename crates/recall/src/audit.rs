//! `recall audit`: holding the server to its own history.
//!
//! The server keeps an append-only Merkle log of everything done to it
//! (`docs/reference/api.md`, "Audit"). This machine witnesses it: every
//! pull saves the checkpoint it carries in `~/.recall/audit.json`, and
//! `recall_hooks::audit` says when those are checked and why not in the
//! hooks. These commands are the owner's end of it:
//!
//! - `export` fetches the whole log, checkpoint then leaves, in the file
//!   format `scripts/audit-verify.py` reads, and checks every checkpoint
//!   saved here against the leaves it fetched. It needs admin authority,
//!   because the leaves name every project, file and device.
//! - `verify FILE` checks an export offline, with the same checks as the
//!   script (`recall_wire::audit::verify`), against every checkpoint saved
//!   here that the export is old enough to cover, and any given with
//!   `--checkpoint`. It needs nothing else.
//! - `verify` with no file asks the server to prove its log still extends
//!   every checkpoint saved here: what `recall doctor` does, on its own.
//! - `reset` forgets what was saved for the server, rewrite found or not.
//!
//! **How loudly they fail.** Loudly: these are run by a person asking a
//! question about tampering, and the answer is the exit code. 0 when
//! everything checks out; 1 when something does not (the export fails a
//! check, the log does not extend a saved checkpoint, the server answers
//! without a proof, or no longer keeps a log this machine saw it keep); 2
//! when it could not be checked at all (no server or home configured, no
//! credential, a file that cannot be read, a server that cannot be reached,
//! did not answer in time, or never kept a log). The same three as the
//! script's, so the two can stand in for each other in a script. `reset`
//! answers 0 when it forgot, or there was nothing to forget; 1 when asked
//! and told no; 2 when it could not ask or could not read the file.

use std::fs::File;
use std::io::{self, BufWriter, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Subcommand;
use recall_hooks::audit::{CheckError, Checkpoint, Inconsistency, Saved, Witness, Witnessed};
use recall_hooks::client::{self, Client};
use recall_hooks::{exit, ClientConfig};
use recall_wire::audit::merkle::{self, Tree};
use recall_wire::audit::verify;

use crate::project as proj;

/// Something did not check out.
const FAILED: i32 = 1;

/// It could not be checked at all.
const UNUSABLE: i32 = 2;

/// `recall audit …`.
#[derive(Subcommand)]
pub enum Cmd {
    /// Write the server's whole audit log to a file that recall audit verify
    /// and scripts/audit-verify.py both read. Needs an admin device or
    /// RECALL_TOKEN
    Export {
        /// Where to write it; standard output when not given
        #[arg(long, short, value_name = "FILE")]
        output: Option<PathBuf>,
    },
    /// Check an export offline. With no file, have the server prove its log
    /// still extends every checkpoint saved here
    Verify {
        /// An export, from recall audit export
        file: Option<PathBuf>,
        /// A checkpoint saved elsewhere, as SIZE:ROOT, that the log must
        /// still extend; may be given more than once
        #[arg(long = "checkpoint", value_name = "SIZE:ROOT")]
        checkpoints: Vec<String>,
    },
    /// Forget the checkpoints saved here for the server, and any rewrite
    /// found in them. For once you know why the log changed
    Reset {
        /// Forget without asking, for scripts
        #[arg(long, short)]
        yes: bool,
    },
}

/// Runs one `recall audit` command.
pub async fn run(cmd: Cmd) -> anyhow::Result<i32> {
    let cfg = proj::resolve().config();
    Ok(match cmd {
        Cmd::Export { output } => export(&cfg, output.as_deref()).await,
        Cmd::Verify {
            file: Some(file),
            checkpoints,
        } => verify_file(&cfg, &file, &checkpoints),
        Cmd::Verify {
            file: None,
            checkpoints,
        } if !checkpoints.is_empty() => refuse(
            "--checkpoint holds an export to a checkpoint, and no export was named.",
            "recall audit verify FILE --checkpoint SIZE:ROOT",
        ),
        Cmd::Verify { file: None, .. } => check(&cfg).await,
        Cmd::Reset { yes } => reset(&cfg, yes),
    })
}

/// This machine's record of the server in effect, when there is a server
/// and somewhere to keep one.
fn witness(cfg: &ClientConfig) -> Result<Witness, i32> {
    if cfg.url.is_empty() {
        return Err(refuse(
            "no server: run recall connect first, or set RECALL_URL.",
            "",
        ));
    }
    match &cfg.audit_file {
        Some(file) => Ok(Witness::new(file, &cfg.url)),
        None => Err(refuse(
            "nowhere to keep checkpoints: neither RECALL_HOME nor HOME is set.",
            "",
        )),
    }
}

// ---------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------

/// How many times a page the server asked to wait for is asked for again,
/// and how long apart: together a little over the server's one-minute
/// rate-limit window, which a long log paged at the full rate can meet.
const RATE_LIMIT_RETRIES: usize = 13;
const RATE_LIMIT_WAIT: Duration = Duration::from_secs(5);

async fn export(cfg: &ClientConfig, output: Option<&Path>) -> i32 {
    let witness = match witness(cfg) {
        Ok(w) => w,
        Err(code) => return code,
    };
    // The leaves are admin-only: an admin device signs, else the token is
    // sent when this machine has one, as for `recall devices`.
    let client = match crate::devices::admin_client(cfg) {
        Ok(client) => client,
        Err(why) => return refuse(&why, ""),
    };
    let capability = match client.discover().await {
        Ok(Some(doc)) => doc.audit(),
        Ok(None) => None,
        Err(e) => return server_error(&e),
    };
    let Some(capability) = capability else {
        return no_log();
    };
    let answer = match client.audit_checkpoint().await {
        Ok(answer) => answer,
        Err(e) => return server_error(&e),
    };
    let Some(current) = Checkpoint::from_wire(&answer) else {
        eprintln!(
            "recall audit: the server's checkpoint is not a size and a root: {}",
            answer.to_header_value()
        );
        return FAILED;
    };

    let mut sink = match Sink::open(output) {
        Ok(sink) => sink,
        Err(e) => {
            eprintln!("recall audit: cannot write {}: {e}", describe(output));
            return UNUSABLE;
        }
    };
    let mut tree = Tree::new();
    let fetched = fetch(&client, current, capability.max_page, &mut sink, &mut tree).await;
    let written = fetched.and_then(|()| sink.finish().map_err(|e| Fetch::Write(e.to_string())));
    if let Err(stop) = written {
        return stop.report(output, cfg);
    }

    if tree.root() != current.root {
        eprintln!(
            "recall audit: FAIL: the leaves the server sent do not hash to its own checkpoint \
             ({}): the export was written, and recall audit verify will say the same",
            current.header()
        );
        return FAILED;
    }
    let said = match witness.witness_export(&tree, current) {
        Ok(Witnessed::Extends { proved, .. }) => match proved {
            0 => "no checkpoint was saved here to hold it to; this one now is".to_string(),
            n => format!("it extends the {n} checkpoint(s) saved here"),
        },
        Ok(Witnessed::Inconsistent { finding, unsaved }) => {
            eprintln!(
                "recall audit: wrote {} leaves to {}",
                current.size,
                describe(output)
            );
            report_inconsistency(&finding, unsaved.as_deref());
            return FAILED;
        }
        Err(e) => {
            // The export is whole; what it could not be held to is the
            // checkpoints saved here, which is not a clean answer.
            eprintln!(
                "recall audit: wrote {} leaves at checkpoint {} to {}, but the checkpoints \
                 saved here could not be checked against it: {e}",
                current.size,
                current.header(),
                describe(output)
            );
            return UNUSABLE;
        }
    };
    eprintln!(
        "recall audit: wrote {} leaves at checkpoint {} to {}; {said}.",
        current.size,
        current.header(),
        describe(output)
    );
    if let Some(path) = output {
        eprintln!(
            "  Check it offline with: recall audit verify {}",
            path.display()
        );
    }
    exit::OK
}

/// Why an export stopped partway.
enum Fetch {
    Server(client::Error),
    Page(String),
    Write(String),
}

impl Fetch {
    fn report(self, output: Option<&Path>, cfg: &ClientConfig) -> i32 {
        match self {
            Fetch::Server(client::Error::Status { code: 403, .. }) => {
                let what = match cfg.device.as_ref() {
                    Some(d) => format!("this machine is enrolled as a {} device", d.scope),
                    None => "the credential this machine sent is not one".to_string(),
                };
                // Refused, as `recall devices` is refused: 2, the server's no.
                refuse(
                    &format!(
                        "reading the log's leaves needs an admin device or the server's \
                         RECALL_TOKEN, and {what}."
                    ),
                    "Run this on an admin device, or with RECALL_TOKEN set. recall audit \
                     verify, with no file, needs neither.",
                );
                UNUSABLE
            }
            Fetch::Server(e) => server_error(&e),
            Fetch::Page(why) => {
                eprintln!("recall audit: the server answered a page that is not one: {why}");
                FAILED
            }
            Fetch::Write(why) => {
                eprintln!("recall audit: cannot write {}: {why}", describe(output));
                UNUSABLE
            }
        }
    }
}

/// Pages through every leaf up to `current`, `max_page` at a time, writing
/// each and adding it to `tree`. A page may stop early at the server's
/// byte limit; the next one starts where it stopped.
async fn fetch(
    client: &Client,
    current: Checkpoint,
    max_page: u32,
    sink: &mut Sink,
    tree: &mut Tree,
) -> Result<(), Fetch> {
    let write = |sink: &mut Sink, bytes: &[u8]| {
        sink.writer()
            .write_all(bytes)
            .map_err(|e| Fetch::Write(e.to_string()))
    };
    let header = format!("{}\n", current.header());
    if current.size == 0 {
        write(sink, header.as_bytes())?;
    }
    let page = u64::from(max_page.max(1));
    let mut start = 0;
    while start < current.size {
        let end = (start + page).min(current.size);
        let got = page_of(client, start, end).await.map_err(Fetch::Server)?;
        // Only once the first page has come: a refusal, which is what a
        // machine without admin authority gets, then leaves nothing on
        // standard output to be mistaken for an export.
        if start == 0 {
            write(sink, header.as_bytes())?;
        }
        let count = got.entries.len() as u64;
        if got.start != start || got.end != start + count || count == 0 || got.end > end {
            return Err(Fetch::Page(format!(
                "asked for {start} to {end}, got {} to {} holding {count}",
                got.start, got.end
            )));
        }
        for leaf in &got.entries {
            if leaf.contains('\n') {
                return Err(Fetch::Page(format!(
                    "leaf {} holds a line break, which no leaf the server writes does",
                    tree.size()
                )));
            }
            tree.append(merkle::hash_leaf(leaf.as_bytes()));
            write(sink, leaf.as_bytes())?;
            write(sink, b"\n")?;
        }
        start = got.end;
    }
    Ok(())
}

/// One page, asked for again while the server's rate limit says to wait.
async fn page_of(
    client: &Client,
    start: u64,
    end: u64,
) -> Result<recall_wire::AuditEntriesResponse, client::Error> {
    let mut waited = 0;
    loop {
        match client.audit_entries(start, end).await {
            Err(client::Error::Status { code: 429, .. }) if waited < RATE_LIMIT_RETRIES => {
                if waited == 0 {
                    eprintln!("recall audit: the server asked to wait (its rate limit); waiting");
                }
                waited += 1;
                tokio::time::sleep(RATE_LIMIT_WAIT).await;
            }
            other => return other,
        }
    }
}

/// Where an export is written: standard output, or a file that appears
/// under its own name only once it is whole.
enum Sink {
    Stdout(BufWriter<io::Stdout>),
    File {
        /// Taken, and so closed, before the rename: Windows will not
        /// rename a file that is still open.
        writer: Option<BufWriter<File>>,
        partial: PathBuf,
        path: PathBuf,
    },
}

impl Sink {
    fn open(output: Option<&Path>) -> io::Result<Self> {
        Ok(match output {
            None => Sink::Stdout(BufWriter::new(io::stdout())),
            Some(path) => {
                let mut partial = path.as_os_str().to_owned();
                partial.push(".partial");
                let partial = PathBuf::from(partial);
                // Created, never opened: a link left at that name (by an
                // earlier export, or planted) is removed, not written
                // through to wherever it points.
                let create = || {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&partial)
                };
                let file = match create() {
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                        std::fs::remove_file(&partial)?;
                        create()?
                    }
                    other => other?,
                };
                Sink::File {
                    writer: Some(BufWriter::new(file)),
                    partial,
                    path: path.to_path_buf(),
                }
            }
        })
    }

    fn writer(&mut self) -> &mut dyn Write {
        match self {
            Sink::Stdout(w) => w,
            Sink::File { writer, .. } => writer.as_mut().expect("open until finished"),
        }
    }

    fn finish(&mut self) -> io::Result<()> {
        match self {
            Sink::Stdout(w) => w.flush(),
            Sink::File {
                writer,
                partial,
                path,
            } => {
                let file = writer
                    .take()
                    .expect("finished once")
                    .into_inner()
                    .map_err(|e| e.into_error())?;
                file.sync_all()?;
                drop(file);
                std::fs::rename(&*partial, &*path)
            }
        }
    }
}

impl Drop for Sink {
    /// An export that stopped partway leaves no file behind that could be
    /// mistaken for a whole one.
    fn drop(&mut self) {
        if let Sink::File {
            writer, partial, ..
        } = self
        {
            // Closed first, for Windows, which will not remove an open file.
            drop(writer.take());
            let _ = std::fs::remove_file(partial);
        }
    }
}

fn describe(output: Option<&Path>) -> String {
    output.map_or_else(
        || "standard output".to_string(),
        |p| p.display().to_string(),
    )
}

// ---------------------------------------------------------------------------
// verify
// ---------------------------------------------------------------------------

fn verify_file(cfg: &ClientConfig, file: &Path, args: &[String]) -> i32 {
    let mut given = Vec::new();
    for arg in args {
        match verify::parse_checkpoint_arg(arg) {
            Some(cp) => given.push(cp),
            None => {
                eprintln!(
                    "recall audit: --checkpoint {arg:?} is not SIZE:ROOT, a size and a root in \
                     standard base64"
                );
                return UNUSABLE;
            }
        }
    }
    let export = match std::fs::read(file) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("recall audit: cannot read {}: {e}", file.display());
            return UNUSABLE;
        }
    };

    // The checkpoints this machine saved for the server in effect, those
    // the export is long enough to hold: an export is a snapshot, and one
    // taken before a checkpoint was saved says nothing about it. A log cut
    // back below one is what `export` and `verify` with no file catch.
    let leaves = leaf_lines(&export);
    let (held, origin) = match (&cfg.audit_file, cfg.url.is_empty()) {
        (Some(path), false) => {
            let witness = Witness::new(path, &cfg.url);
            match witness.load() {
                Ok(saved) => (saved.all(), Some(witness.origin().to_string())),
                Err(e) => {
                    eprintln!("recall audit: {e}, so the checkpoints saved here cannot be checked");
                    return UNUSABLE;
                }
            }
        }
        _ => (Vec::new(), None),
    };
    let (covered, newer): (Vec<Checkpoint>, Vec<Checkpoint>) =
        held.iter().partition(|c| c.size <= leaves);
    let saved: Vec<(u64, merkle::Hash)> = covered
        .iter()
        .map(|c| (c.size, c.root))
        .chain(given.iter().copied())
        .collect();

    let verdict = verify::verify_export(&export, &saved);
    if !verdict.ok() {
        for problem in &verdict.problems {
            eprintln!("FAIL: {problem}");
        }
        return FAILED;
    }
    let mut held_to = Vec::new();
    if let Some(origin) = &origin {
        if !covered.is_empty() {
            held_to.push(format!(
                "the {} checkpoint(s) saved here for {origin}",
                covered.len()
            ));
        }
    }
    if !given.is_empty() {
        held_to.push(format!("the {} given with --checkpoint", given.len()));
    }
    let held_to = match held_to.is_empty() {
        true => "no saved checkpoint to hold it to".to_string(),
        false => format!("it extends {}", held_to.join(" and ")),
    };
    println!(
        "OK: checkpoint {}; {} leaves, {} signed, every signature checked; {held_to}",
        verdict.checkpoint, verdict.leaves, verdict.signed
    );
    if !newer.is_empty() {
        println!(
            "  {} checkpoint(s) saved here are newer than this export; recall audit verify, \
             with no file, checks those against the server",
            newer.len()
        );
    }
    exit::OK
}

/// How many leaf lines follow an export's checkpoint line, counted as
/// `verify_export` counts them: one trailing newline ends the last line.
fn leaf_lines(export: &[u8]) -> u64 {
    let body = export.strip_suffix(b"\n").unwrap_or(export);
    if body.is_empty() {
        return 0;
    }
    body.iter().filter(|&&b| b == b'\n').count() as u64
}

/// `verify` with no file: the check `recall doctor` makes, alone.
async fn check(cfg: &ClientConfig) -> i32 {
    let witness = match witness(cfg) {
        Ok(w) => w,
        Err(code) => return code,
    };
    let client = match cfg.client() {
        Ok(client) => client,
        Err(e) => return refuse(&e.to_string(), ""),
    };
    let saved = match witness.load() {
        Ok(saved) => saved.all().len(),
        Err(e) => return unreadable(&e.to_string()),
    };
    match witness.check(&client, CHECK_DEADLINE).await {
        Ok(Witnessed::Extends { current, proved }) => {
            let kept = witness.load().map(|s| s.checkpoints.len()).unwrap_or(0);
            println!(
                "OK: the log at {} has {} leaves (root {}) and extends every checkpoint saved \
                 here: {proved} proven now, {kept} kept",
                witness.origin(),
                current.size,
                checkpoint_root(&current)
            );
            exit::OK
        }
        Ok(Witnessed::Inconsistent { finding, unsaved }) => {
            report_inconsistency(&finding, unsaved.as_deref());
            FAILED
        }
        // A log this machine witnessed and the server no longer keeps is a
        // history lost, as `recall doctor` says; one never kept is not.
        Err(e) if e.no_log() && saved > 0 => {
            eprintln!(
                "FAIL: the server keeps no audit log, and this machine saved {saved} \
                 checkpoint(s) of one: a server that went back to before 0.4.2 lost it"
            );
            eprintln!("  {AFTER_A_REWRITE}");
            FAILED
        }
        Err(e) if e.no_log() => no_log(),
        Err(e) if e.unreadable() => unreadable(&e.to_string()),
        Err(e) if e.unanswered() => {
            eprintln!("recall audit: {e}; what was proven before then is kept");
            UNUSABLE
        }
        Err(e @ CheckError::File(_)) => {
            eprintln!("recall audit: {e}");
            UNUSABLE
        }
        // The server answered, with something that is not a proof.
        Err(e) => {
            eprintln!(
                "FAIL: the server did not prove its log extends the checkpoints saved here: {e}"
            );
            FAILED
        }
    }
}

/// How long `recall audit verify` with no file gives the server, its rate
/// limit's pauses included: longer than `recall doctor`, since someone
/// asked for this check and nothing else, and a little over the server's
/// one-minute rate-limit window.
const CHECK_DEADLINE: Duration = Duration::from_secs(90);

/// `audit.json` could not be read: it may hold the only record of a
/// rewrite, so it is said as loudly as one, and nothing writes over it.
fn unreadable(why: &str) -> i32 {
    eprintln!(
        "recall audit: {why}; it may hold the only record of a rewrite, so nothing was checked \
         or saved"
    );
    eprintln!("  Look at it first; move it aside only once you know what it held.");
    UNUSABLE
}

fn checkpoint_root(cp: &Checkpoint) -> String {
    cp.header()
        .split_once(' ')
        .map(|(_, root)| root.to_string())
        .unwrap_or_default()
}

/// What an inconsistency looks like on a terminal, and what to do about
/// it, which depends on whether the owner knows why.
pub(crate) fn report_inconsistency(found: &Inconsistency, unsaved: Option<&str>) {
    eprintln!(
        "FAIL: the server's audit log no longer extends a checkpoint this machine saved: {}",
        found.detail
    );
    eprintln!("  found  {}", found.found_at);
    eprintln!("  saved  {}", found.saved_header());
    eprintln!("  seen   {}", found.seen_header());
    if let Some(why) = unsaved {
        eprintln!("  NOT SAVED to audit.json ({why}): keep this output");
    }
    eprintln!("  {}", AFTER_A_REWRITE);
}

/// The two ways a log that no longer extends a checkpoint came to be.
pub(crate) const AFTER_A_REWRITE: &str =
    "If the server was restored from a backup, that is why: recall audit reset, and it starts \
     again from the log as it is. If not, its history was rewritten: keep the evidence first, \
     recall audit export -o audit-evidence.jsonl";

// ---------------------------------------------------------------------------
// reset
// ---------------------------------------------------------------------------

fn reset(cfg: &ClientConfig, yes: bool) -> i32 {
    let witness = match witness(cfg) {
        Ok(w) => w,
        Err(code) => return code,
    };
    let held = match witness.load() {
        Ok(held) => held,
        Err(e) => return unreadable(&e.to_string()),
    };
    if held.is_empty() {
        println!("Nothing is saved here for {}.", witness.origin());
        return exit::OK;
    }
    let what = describe_saved(&held);
    if !yes {
        if !(io::stdin().is_terminal() && io::stderr().is_terminal()) {
            return refuse("needs a terminal to ask first.", "In a script, pass --yes.");
        }
        let question = format!(
            "Forget {what} for {}? Do this once you know why the log changed.",
            witness.origin()
        );
        match cliclack::confirm(question).initial_value(false).interact() {
            Ok(true) => {}
            // Asked, and not done.
            _ => return FAILED,
        }
    }
    match witness.reset() {
        Ok(_) => {
            println!(
                "Forgot {what} for {}. The next pull saves a first checkpoint again.",
                witness.origin()
            );
            exit::OK
        }
        Err(e) => refuse(&e.to_string(), ""),
    }
}

fn describe_saved(held: &Saved) -> String {
    let n = held.checkpoints.len() + held.unchecked.len();
    let mut what = format!("{n} checkpoint(s)");
    if let Some(found) = &held.inconsistent {
        what.push_str(&format!(" and the rewrite found on {}", found.found_at));
    }
    what
}

// ---------------------------------------------------------------------------
// saying no
// ---------------------------------------------------------------------------

/// Stops before anything could be checked, with the reason: 2, as the
/// script says it could not check as asked, whether what is missing is a
/// server, a home for `audit.json`, a credential or an argument.
fn refuse(what: &str, then: &str) -> i32 {
    eprintln!("recall audit: {what}");
    if !then.is_empty() {
        eprintln!("  {then}");
    }
    UNUSABLE
}

fn no_log() -> i32 {
    eprintln!("recall audit: the server keeps no audit log: it is older than 0.4.2.");
    UNUSABLE
}

fn server_error(e: &client::Error) -> i32 {
    eprintln!("recall audit: {}", e.reason());
    if e.device_gone() {
        eprintln!("  Run recall connect to enrol this machine again.");
    } else if matches!(e, client::Error::Transport(_)) {
        eprintln!("  Check the server is up: recall doctor");
    }
    UNUSABLE
}
