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

use crate::devices::{count, done, next_line, relative, short_hash, title_on_stderr, wrap};
use crate::edit::printable;
use crate::project as proj;
use crate::ui::{self, Tone};

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
        return no_log_to_export(&witness);
    };
    let answer = match client.audit_checkpoint().await {
        Ok(answer) => answer,
        Err(client::Error::Status { code: 404, .. }) => return no_log_to_export(&witness),
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

    // Said on stderr, all of it: standard output may be the export itself.
    let wrote = format!(
        "wrote {} to {}",
        count(current.size as usize, "leaf", "leaves"),
        describe(output)
    );
    let at = format!("at {}", describe_checkpoint(&current));
    if tree.root() != current.root {
        title_on_stderr("recall audit export", witness.origin());
        eprintln!();
        mark_line(Tone::Good, &wrote);
        detail(&at);
        mark_line(
            Tone::Bad,
            "the leaves the server sent do not hash to its own checkpoint",
        );
        detail("recall audit verify says the same of the file.");
        return FAILED;
    }
    let held = match witness.witness_export(&tree, current) {
        Ok(Witnessed::Extends { proved, .. }) => proved,
        Ok(Witnessed::Inconsistent { finding, unsaved }) => {
            title_on_stderr("recall audit export", witness.origin());
            eprintln!();
            mark_line(Tone::Good, &wrote);
            detail(&at);
            let kept = describe(output);
            report_inconsistency(&finding, unsaved.as_deref(), Some(&kept));
            return FAILED;
        }
        Err(e) => {
            // The export is whole; what it could not be held to is the
            // checkpoints saved here, which is not a clean answer.
            eprintln!(
                "recall audit: {wrote}, {at}, but the checkpoints saved here could not be \
                 checked against it: {e}"
            );
            return UNUSABLE;
        }
    };
    title_on_stderr("recall audit export", witness.origin());
    eprintln!();
    mark_line(Tone::Good, &wrote);
    detail(&at);
    match held {
        0 => mark_line(
            Tone::Quiet,
            "no checkpoint was saved here to hold it to; this one now is",
        ),
        n => mark_line(
            Tone::Good,
            &format!(
                "it extends the {} saved here",
                count(n, "checkpoint", "checkpoints")
            ),
        ),
    }
    if let Some(path) = output {
        next_on_stderr(
            &format!("recall audit verify {}", path.display()),
            "checks it offline",
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
    let file_name = file.display().to_string();
    if !verdict.ok() {
        // The answer, on stderr as the script says it: every problem, each
        // hash in it cut short, then how many.
        title_on_stderr("recall audit verify", &file_name);
        eprintln!();
        for problem in &verdict.problems {
            mark_line(Tone::Bad, &printable(&shorten_hashes(problem)));
        }
        verdict_on_stderr(
            Tone::Bad,
            &format!(
                "The export does not check out: {}.",
                count(verdict.problems.len(), "problem", "problems")
            ),
        );
        return FAILED;
    }
    let mut held_to = Vec::new();
    if let Some(origin) = &origin {
        if !covered.is_empty() {
            held_to.push(format!(
                "the {} saved here for {origin}",
                count(covered.len(), "checkpoint", "checkpoints")
            ));
        }
    }
    if !given.is_empty() {
        held_to.push(format!("the {} given with --checkpoint", given.len()));
    }

    ui::title("recall audit verify", &file_name);
    anstream::println!();
    // Checked, so it reads; the line as written should it somehow not.
    let root = Checkpoint::from_header(&verdict.checkpoint)
        .map(|cp| checkpoint_root(&cp))
        .unwrap_or_else(|| verdict.checkpoint.clone());
    ui::check(
        Tone::Good,
        "leaves",
        LABEL_WIDTH,
        &format!(
            "{}, whose root is the checkpoint's: {}",
            verdict.leaves,
            short_hash(&root)
        ),
        None,
    );
    ui::check(
        Tone::Good,
        "signatures",
        LABEL_WIDTH,
        &format!(
            "every one checked, on {}",
            count(verdict.signed as usize, "signed leaf", "signed leaves")
        ),
        None,
    );
    if held_to.is_empty() {
        ui::check(
            Tone::Quiet,
            "checkpoints",
            LABEL_WIDTH,
            "none saved here or given to hold it to",
            None,
        );
    } else {
        ui::check(
            Tone::Good,
            "checkpoints",
            LABEL_WIDTH,
            &format!("it extends {}", held_to.join(" and ")),
            None,
        );
    }
    if !newer.is_empty() {
        ui::check(
            Tone::Quiet,
            "newer",
            LABEL_WIDTH,
            &format!(
                "{} saved here {} newer than this export",
                count(newer.len(), "checkpoint", "checkpoints"),
                if newer.len() == 1 { "is" } else { "are" }
            ),
            Some("recall audit verify (no file) checks them against the server"),
        );
    }
    ui::verdict(Tone::Good, "The export checks out.");
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
            ui::title("recall audit verify", witness.origin());
            anstream::println!();
            anstream::println!(
                "  {} The server's log extends every checkpoint saved here.",
                ui::toned(Tone::Good, Tone::Good.mark())
            );
            anstream::println!(
                "    {}",
                ui::dim(&format!(
                    "{proved} proven now, {kept} kept · the log now: {}",
                    describe_checkpoint(&current)
                ))
            );
            exit::OK
        }
        Ok(Witnessed::Inconsistent { finding, unsaved }) => {
            title_on_stderr("recall audit verify", witness.origin());
            eprintln!();
            report_inconsistency(&finding, unsaved.as_deref(), None);
            FAILED
        }
        // A log this machine witnessed and the server no longer keeps is a
        // history lost, as `recall doctor` says; one never kept is not.
        Err(e) if e.no_log() && saved > 0 => {
            title_on_stderr("recall audit verify", witness.origin());
            eprintln!();
            lost_log(saved)
        }
        Err(e) if e.no_log() => no_log(),
        Err(e) if e.unreadable() => unreadable(&e.to_string()),
        // Before `unanswered`, which it is part of: what to do is about
        // the credential, not the server.
        Err(e) if e.refused() => {
            eprintln!("recall audit: the server refused this machine's credential: {e}");
            eprintln!(
                "  The device may have been revoked, or not be allowed the audit routes: \
                 recall status says which, and recall connect enrols it again."
            );
            UNUSABLE
        }
        Err(e) if e.unanswered() => {
            eprintln!("recall audit: {e}; what was proven before then is kept");
            UNUSABLE
        }
        Err(e @ (CheckError::File(_) | CheckError::Moved)) => {
            eprintln!("recall audit: {e}");
            UNUSABLE
        }
        // The server answered, with something that is not a proof.
        Err(e) => {
            title_on_stderr("recall audit verify", witness.origin());
            eprintln!();
            mark_line(
                Tone::Bad,
                "the server did not prove its log extends the checkpoints saved here",
            );
            detail(&e.to_string());
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

/// `checkpoint 17 · root P7nycHqp…`: a checkpoint for a person to read,
/// its root cut short. [`Checkpoint::header`] is the whole of it.
fn describe_checkpoint(cp: &Checkpoint) -> String {
    format!(
        "checkpoint {} · root {}",
        cp.size,
        short_hash(&checkpoint_root(cp))
    )
}

/// What an inconsistency looks like on a terminal, and what to do about
/// it, which depends on whether the owner knows why. The checkpoints are
/// written whole, and the time it was found exactly as well: they are the
/// evidence, and this may be the only copy of it.
pub(crate) fn report_inconsistency(
    found: &Inconsistency,
    unsaved: Option<&str>,
    kept: Option<&str>,
) {
    mark_line(
        Tone::Bad,
        "the server's audit log no longer extends a checkpoint this machine saved",
    );
    detail(&found.detail);
    let at = format!("{} ({})", relative(&found.found_at), found.found_at);
    for (label, value) in [
        ("found", at),
        ("saved", found.saved_header()),
        ("seen", found.seen_header()),
    ] {
        anstream::eprintln!("    {}  {value}", ui::dim(&format!("{label:<5}")));
    }
    if let Some(why) = unsaved {
        mark_line(
            Tone::Warn,
            &format!("NOT SAVED to audit.json ({why}): keep this output"),
        );
    }
    after_a_rewrite(kept);
}

/// [`AFTER_A_REWRITE`], as the two next steps it names. An export has
/// just written the evidence to `kept`, so it says to keep that rather
/// than to export again.
fn after_a_rewrite(kept: Option<&str>) {
    eprintln!();
    detail("If the server was restored from a backup, that is why:");
    next_on_stderr("recall audit reset", "starts again from the log as it is");
    match kept {
        Some(kept) => detail(&format!(
            "If not, its history was rewritten: keep {kept}, the evidence."
        )),
        None => {
            detail("If not, its history was rewritten. Keep the evidence first:");
            next_on_stderr("recall audit export -o audit-evidence.jsonl", "");
        }
    }
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
        anstream::println!(
            "{} Nothing to forget: nothing is saved here for {}.",
            ui::toned(Tone::Quiet, Tone::Quiet.mark()),
            witness.origin()
        );
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
            done(&format!("Forgot {what}, for {}.", witness.origin()));
            anstream::println!("  The next pull saves a first checkpoint again.");
            exit::OK
        }
        Err(e) => refuse(&e.to_string(), ""),
    }
}

fn describe_saved(held: &Saved) -> String {
    let n = held.checkpoints.len() + held.unchecked.len();
    let mut what = count(n, "checkpoint", "checkpoints");
    if let Some(found) = &held.inconsistent {
        what.push_str(&format!(
            " and the rewrite found {}",
            relative(&found.found_at)
        ));
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

/// The server keeps no audit log to export: nothing to check when this
/// machine never saw it keep one (2), and a history lost when it did (1),
/// as `verify` and `recall doctor` say.
fn no_log_to_export(witness: &Witness) -> i32 {
    match witness.load() {
        Ok(saved) if !saved.all().is_empty() => {
            title_on_stderr("recall audit export", witness.origin());
            eprintln!();
            lost_log(saved.all().len())
        }
        Ok(_) => no_log(),
        Err(e) => unreadable(&e.to_string()),
    }
}

/// A log this machine saved `saved` checkpoints of, which the server no
/// longer keeps: 1.
fn lost_log(saved: usize) -> i32 {
    mark_line(
        Tone::Bad,
        &format!(
            "the server keeps no audit log, and this machine saved {} of one",
            count(saved, "checkpoint", "checkpoints")
        ),
    );
    detail("A server that went back to before 0.4.2 lost it.");
    after_a_rewrite(None);
    FAILED
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

// ---------------------------------------------------------------------------
// how an answer reads
// ---------------------------------------------------------------------------

/// How wide an answer's lines may run before they wrap.
const WIDTH: usize = 100;

/// How wide the labels of `recall audit verify FILE`'s checks are: as wide
/// as `checkpoints`, the widest.
const LABEL_WIDTH: usize = 11;

/// One marked line of an answer on stderr, wrapped under its own text.
fn mark_line(tone: Tone, text: &str) {
    for (i, line) in wrap(text, WIDTH - 4).iter().enumerate() {
        match i {
            0 => anstream::eprintln!("  {} {line}", ui::toned(tone, tone.mark())),
            _ => anstream::eprintln!("    {line}"),
        }
    }
}

/// A dimmed line under a marked one, on stderr.
fn detail(text: &str) {
    for line in wrap(text, WIDTH - 4) {
        anstream::eprintln!("    {}", ui::dim(&line));
    }
}

/// The command to run next, and what it does, on stderr.
fn next_on_stderr(command: &str, what: &str) {
    anstream::eprintln!("{}", next_line(command, what));
}

/// [`ui::verdict`], on stderr: the closing line of an answer that did not
/// check out.
fn verdict_on_stderr(tone: Tone, text: &str) {
    anstream::eprintln!();
    anstream::eprintln!("{} {}", ui::toned(tone, tone.mark()), ui::bold(text));
}

/// `text` with every tree hash in it, 32 bytes in standard base64, cut to
/// its first eight characters: a problem `verify_export` found reads in a
/// line, and the hashes are there in full in the file and in `audit.json`
/// to look at again.
fn shorten_hashes(text: &str) -> String {
    let is_b64 = |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=');
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if word.len() == 44 && word.ends_with('=') && verify::root_hash(word).is_some() {
            out.push_str(&short_hash(word));
        } else {
            out.push_str(word);
        }
        word.clear();
    };
    for c in text.chars() {
        if is_b64(c) {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A problem reads in a line: its roots are cut short, and nothing else
    /// in it is touched, a word of base64 that is not a root included.
    #[test]
    fn a_problems_roots_are_cut_short_and_nothing_else() {
        let problem = "the root over the first 16 leaves is \
                       A9eynh+FB8idl91nn/0ibY0DxDqTpcrMv7JstDslA+g=, the saved checkpoint says \
                       gBzXgBzaYCPdFUGxq2R1UtJhM9rS104gua1Tw/Xp/Q8=: the log does not extend it";
        assert_eq!(
            shorten_hashes(problem),
            "the root over the first 16 leaves is A9eynh+F…, the saved checkpoint says \
             gBzXgBza…: the log does not extend it"
        );
        let other = "leaf 3: signature x4bsQ2 does not verify";
        assert_eq!(shorten_hashes(other), other);
    }
}
