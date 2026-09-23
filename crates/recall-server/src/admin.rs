//! `recall-server admin`: listing, renaming, removing and restoring the
//! memory stored under a project key, from a shell on the server's host.
//!
//! ```text
//! docker exec -it -u node recall-server recall-server admin list
//! ```
//!
//! It is a subcommand rather than a route on purpose. The HTTP API has no
//! way to delete or move a project, so a leaked token cannot destroy history
//! through it, and this keeps that true: these commands open no listener of
//! any kind, not even one bound to `127.0.0.1`. Running them takes a shell
//! inside the server's container, which is the same access that could
//! already open the database file directly, so they give no attacker
//! anything new. "Admin commands" in `ARCHITECTURE.md` has the reasoning.
//!
//! What they add over the file is the procedure a human used to have to
//! remember. Every change:
//!
//! 1. names its keys exactly (never a prefix or a pattern) and is confirmed
//!    by typing each key back, a rename's target included, or by `--yes`,
//!    which prints them instead;
//! 2. takes a fresh backup first, with [`Store::backup`], into an `admin/`
//!    directory the server's rotation never prunes, checks that the backup
//!    holds the rows it was shown, and prints its path;
//! 3. runs in one transaction, re-checks inside it that nothing changed
//!    since it was shown, and commits only if `changes()` matches;
//! 4. waits out the server's merge window after committing, and checks that
//!    no push that was already in flight has partly undone it;
//! 5. can be previewed with `--dry-run`, which changes nothing and takes no
//!    backup.
//!
//! It runs beside a running server. Both wait up to 5 seconds on a lock the
//! other holds, and a change's transaction takes the write lock before it
//! re-reads anything, so neither can fail the other half way. A lock cannot
//! order a change against a whole push, though, which reads, merges with no
//! lock held, and then writes: step 4 is there for the push that read before
//! the change and writes after it. The store's admin module has the detail.

use std::borrow::Cow;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::store::admin::{archive_key, invisible, Abandoned, Access, Plan, Restore, Row, Summary};
use crate::Store;

const USAGE: &str = "\
Changes the memory a Recall server stores, from a shell on its host.

Usage:
  recall-server admin list [--backup <file>] [<key>]
  recall-server admin rename <from> <to> [--dry-run] [--yes]
  recall-server admin remove <key> [--dry-run] [--yes]
  recall-server admin restore <backup-file> <key> [--overwrite [--restore-deletions]]
                              [--dry-run] [--yes]

  list      Every project key with its files, tombstones and last update.
            With a key, that key's files. With --backup, what a backup holds.
  rename    Move every row of one key to another key that holds none.
  remove    Delete every row of one key.
  restore   Copy one key's rows from a backup into the live database.

Options:
  --dry-run            Print exactly what would change, and change nothing.
  --yes                Confirm without typing the keys. They are printed
                       instead.
  --overwrite          Let restore replace live rows that differ from the
                       backup, except a live file the backup has deleted.
  --restore-deletions  With --overwrite, let restore turn such a live file
                       into a tombstone too, which deletes it on every
                       machine at its next pull.
  --backup             Make list read a backup file instead of the live
                       database.

Keys are matched exactly, never by prefix or pattern. Put -- before a key
that starts with a dash.

Every change takes a backup first, into $RECALL_BACKUP_DIR/admin/, which the
server's rotation never prunes. It runs in one transaction and commits only
if exactly the rows it showed changed. Then it waits out the server's merge
window (RECALL_MERGE_TIMEOUT_MS, plus a second) and checks that no push
already in flight has partly undone it. The database is RECALL_DB_PATH.

Exit status: 0 done, or nothing to do; 1 refused or failed, and nothing was
changed; 2 a usage error; 3 the change was made, but something needs a look
(the message says what).

In the Docker setup, run it as the user the server runs as:
  docker exec -it -u node recall-server recall-server admin list";

/// Runs `recall-server admin`, given the arguments after `admin`.
///
/// Exits 0 on success, including a dry run and a change with nothing to do;
/// 1 when a command is refused or fails, having changed nothing; 2 for a
/// usage error; 3 when the change committed but the check after it found
/// something the owner has to act on.
pub fn main(args: &[String]) -> ExitCode {
    let env = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
    let stdin = io::stdin();
    let stdout = io::stdout();
    let terminal = stdin.is_terminal();
    let command = match parse(args) {
        Ok(command) => command,
        Err(msg) => {
            eprintln!("recall-server admin: {msg}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let changes = command.changes_memory();
    let io = Io {
        input: &mut stdin.lock(),
        terminal,
        out: &mut stdout.lock(),
    };
    match execute(command, &env, io) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) if err.is::<AfterCommit>() => {
            eprintln!("recall-server admin: {err:#}");
            ExitCode::from(3)
        }
        Err(err) => {
            eprintln!("recall-server admin: {err:#}");
            // True of every other error a change can return: after COMMIT
            // there is only printing, whose errors are ignored, and the
            // check after the wait, whose every error is an AfterCommit.
            if changes {
                eprintln!("The database was not changed.");
            }
            ExitCode::FAILURE
        }
    }
}

/// A failure after the change committed: the one kind of error after which
/// "the database was not changed" would be a lie.
#[derive(Debug)]
struct AfterCommit(String);

impl std::fmt::Display for AfterCommit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AfterCommit {}

/// How long after the server's merge timeout a push that timed out can
/// still take to write: the fallback write itself, and scheduling. Generous,
/// because waiting a second too long costs nothing, and checking a second
/// too early misses the very push this wait is for.
const SETTLE_MARGIN: Duration = Duration::from_secs(1);

/// How long a push that was in flight when a change committed can take to
/// land: the server's merge timeout, read the way the server reads it, plus
/// [`SETTLE_MARGIN`].
///
/// The command runs inside the server's container, so it sees the server's
/// own environment. With merge turned off a push reads and writes with
/// nothing to wait for in between, so only the margin is left.
fn settle_window(env: &dyn Fn(&str) -> Option<String>) -> Duration {
    // Both as `Config::from_lookup` has them: only a literal "false" turns
    // merge off, and a timeout that is unparseable or zero means 45 seconds.
    let merging = env("RECALL_MERGE_ENABLED").as_deref() != Some("false");
    let timeout_ms = env("RECALL_MERGE_TIMEOUT_MS")
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(45_000);
    if merging {
        Duration::from_millis(timeout_ms) + SETTLE_MARGIN
    } else {
        SETTLE_MARGIN
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Opts {
    dry_run: bool,
    yes: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Help,
    List {
        backup: Option<PathBuf>,
        key: Option<String>,
    },
    Rename {
        from: String,
        to: String,
        opts: Opts,
    },
    Remove {
        key: String,
        opts: Opts,
    },
    Restore {
        backup: PathBuf,
        key: String,
        overwrite: bool,
        deletions: bool,
        opts: Opts,
    },
}

impl Command {
    fn changes_memory(&self) -> bool {
        matches!(
            self,
            Command::Rename { .. } | Command::Remove { .. } | Command::Restore { .. }
        )
    }
}

fn parse(args: &[String]) -> Result<Command, String> {
    let Some((name, rest)) = args.split_first() else {
        return Err("which command? list, rename, remove or restore".into());
    };
    let allowed: &[&str] = match name.as_str() {
        "help" => &[],
        "list" => &["--backup"],
        "rename" | "remove" => &["--dry-run", "--yes"],
        "restore" => &["--overwrite", "--restore-deletions", "--dry-run", "--yes"],
        other => return Err(format!("unknown command {other:?}")),
    };

    let mut positional = Vec::new();
    let mut flags = Vec::new();
    let mut backup = None;
    let mut rest = rest.iter();
    let mut options_done = false;
    while let Some(arg) = rest.next() {
        if options_done || !arg.starts_with('-') || arg == "-" {
            positional.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => options_done = true,
            "-h" | "--help" => return Ok(Command::Help),
            "--dry-run" | "--yes" | "--overwrite" | "--restore-deletions" => {
                flags.push(arg.as_str())
            }
            "--backup" => {
                backup = Some(rest.next().ok_or("--backup needs a file")?.clone());
                flags.push("--backup");
            }
            other => match other.strip_prefix("--backup=") {
                Some(file) => {
                    backup = Some(file.to_owned());
                    flags.push("--backup");
                }
                None => return Err(format!("unknown option {other}")),
            },
        }
    }
    if let Some(flag) = flags.iter().find(|f| !allowed.contains(f)) {
        return Err(format!("{flag} is not an option of {name}"));
    }
    let has = |flag: &str| flags.contains(&flag);
    // Turning a live file into a tombstone is replacing a live row, so it
    // is asked for on top of permission to replace rows at all, never
    // instead of it.
    if has("--restore-deletions") && !has("--overwrite") {
        return Err("--restore-deletions needs --overwrite as well".into());
    }
    let opts = Opts {
        dry_run: has("--dry-run"),
        yes: has("--yes"),
    };

    let count = positional.len();
    let mut positional = positional.into_iter();
    let mut next = || positional.next().unwrap_or_default();
    match (name.as_str(), count) {
        ("help", 0) => Ok(Command::Help),
        ("list", 0) => Ok(Command::List {
            backup: backup.map(PathBuf::from),
            key: None,
        }),
        ("list", 1) => Ok(Command::List {
            backup: backup.map(PathBuf::from),
            key: Some(next()),
        }),
        ("rename", 2) => Ok(Command::Rename {
            from: next(),
            to: next(),
            opts,
        }),
        ("remove", 1) => Ok(Command::Remove { key: next(), opts }),
        ("restore", 2) => Ok(Command::Restore {
            backup: PathBuf::from(next()),
            key: next(),
            overwrite: has("--overwrite"),
            deletions: has("--restore-deletions"),
            opts,
        }),
        ("help", _) => Err("help takes no arguments".into()),
        ("list", _) => Err("list takes at most one key".into()),
        ("rename", _) => Err("rename takes two keys: <from> <to>".into()),
        ("remove", _) => Err("remove takes one key".into()),
        _ => Err("restore takes a backup file and a key: <backup-file> <key>".into()),
    }
}

/// How a command talks to the person running it.
struct Io<'a> {
    input: &'a mut dyn BufRead,
    /// Whether `input` is a terminal, which echoes the newline a typed
    /// confirmation ends with. Piped input does not, so one is printed.
    terminal: bool,
    out: &'a mut dyn Write,
}

/// Where a command reads and writes, and how it talks to the person
/// running it.
struct Ctx<'a> {
    db: PathBuf,
    backup_dir: Option<PathBuf>,
    /// How long to wait after a commit before checking it held; see
    /// [`settle_window`].
    settle: Duration,
    input: &'a mut dyn BufRead,
    terminal: bool,
    out: &'a mut dyn Write,
}

fn execute(command: Command, env: &dyn Fn(&str) -> Option<String>, io: Io) -> Result<()> {
    let mut ctx = Ctx {
        // The server's own default, so the two cannot disagree about which
        // file is meant. Nothing is created if it is missing.
        db: PathBuf::from(env("RECALL_DB_PATH").unwrap_or_else(|| "data/recall.db".into())),
        backup_dir: env("RECALL_BACKUP_DIR").map(PathBuf::from),
        settle: settle_window(env),
        input: io.input,
        terminal: io.terminal,
        out: io.out,
    };
    match command {
        Command::Help => {
            writeln!(ctx.out, "{USAGE}")?;
            Ok(())
        }
        Command::List { backup, key } => list(&mut ctx, backup.as_deref(), key.as_deref()),
        Command::Rename { from, to, opts } => {
            let store = open_live(&ctx.db, opts)?;
            require_key(&store, &from, LIVE)?;
            let plan = store.plan_rename(&from, &to)?;
            let again = format!("recall-server admin rename {}", sh_args(&[&from, &to]));
            carry_out(&mut ctx, &store, &plan, &again, opts)
        }
        Command::Remove { key, opts } => {
            let store = open_live(&ctx.db, opts)?;
            require_key(&store, &key, LIVE)?;
            let plan = store.plan_remove(&key)?;
            let again = format!("recall-server admin remove {}", sh_args(&[&key]));
            carry_out(&mut ctx, &store, &plan, &again, opts)
        }
        Command::Restore {
            backup,
            key,
            overwrite,
            deletions,
            opts,
        } => {
            if same_file(&backup, &ctx.db) {
                bail!(
                    "{} is the live database itself. Name a backup file: `ls` the backup \
                     directory, or see `recall-server admin list --backup <file>`",
                    backup.display()
                );
            }
            let source = Store::open_existing(&backup, Access::Read)?;
            require_key(&source, &key, BACKUP)?;
            let rows = source.rows(&key)?;
            let store = open_live(&ctx.db, opts)?;
            writeln!(ctx.out, "Backup: {}", backup.display())?;
            let plan = store.plan_restore(&key, rows, overwrite, deletions)?;
            let again = format!(
                "recall-server admin restore --overwrite{} {}",
                if deletions {
                    " --restore-deletions"
                } else {
                    ""
                },
                sh_args(&[&backup.to_string_lossy(), &key])
            );
            carry_out(&mut ctx, &store, &plan, &again, opts)
        }
    }
}

/// Positional arguments as shell words, for a command line the owner is
/// told to run: each as is when that is unambiguous and single-quoted
/// otherwise, all after `--` when one starts with a dash (see USAGE).
fn sh_args(args: &[&str]) -> String {
    let words: Vec<String> = args
        .iter()
        .map(|s| {
            let plain = !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./:@%+=,".contains(c));
            if plain {
                s.to_string()
            } else {
                format!("'{}'", s.replace('\'', r"'\''"))
            }
        })
        .collect();
    let dash = if args.iter().any(|s| s.starts_with('-')) {
        "-- "
    } else {
        ""
    };
    format!("{dash}{}", words.join(" "))
}

fn list(ctx: &mut Ctx, backup: Option<&Path>, key: Option<&str>) -> Result<()> {
    let (label, path) = match backup {
        Some(file) => ("Backup", file),
        None => ("Database", ctx.db.as_path()),
    };
    let store = Store::open_existing(path, Access::Read)?;
    let out = &mut *ctx.out;
    writeln!(out, "{label}: {}", path.display())?;

    let Some(key) = key else {
        let summaries = store.summaries()?;
        if summaries.is_empty() {
            writeln!(out, "No project keys.")?;
            return Ok(());
        }
        writeln!(
            out,
            "{:>7}  {:>10}  {:<24}  PROJECT KEY",
            "FILES", "TOMBSTONES", "LAST UPDATE"
        )?;
        for s in &summaries {
            writeln!(
                out,
                "{:>7}  {:>10}  {:<24}  {}",
                s.files,
                s.tombstones,
                s.last_updated_at,
                shown(&s.project_key)
            )?;
        }
        let files: i64 = summaries.iter().map(|s| s.files).sum();
        let tombstones: i64 = summaries.iter().map(|s| s.tombstones).sum();
        writeln!(
            out,
            "{} project key(s), {files} file(s), {tombstones} tombstone(s).",
            summaries.len()
        )?;
        return Ok(());
    };

    let what = if backup.is_some() { BACKUP } else { LIVE };
    require_key(&store, key, what)?;
    let rows = store.rows(key)?;
    let tombstones = rows.iter().filter(|r| r.deleted).count();
    writeln!(
        out,
        "Project key {key:?}: {} file(s), {tombstones} tombstone(s).",
        rows.len() - tombstones
    )?;
    let source_width = rows
        .iter()
        .map(|r| source(r).chars().count())
        .max()
        .unwrap_or(0)
        .max("SOURCE".len());
    writeln!(
        out,
        "{:<9}  {:>8}  {:<24}  {:<source_width$}  PATH",
        "STATE", "BYTES", "UPDATED", "SOURCE"
    )?;
    for r in &rows {
        writeln!(
            out,
            "{:<9}  {:>8}  {:<24}  {:<source_width$}  {}",
            state(r),
            r.content.len(),
            r.updated_at,
            source(r),
            shown(&r.file_path)
        )?;
    }
    Ok(())
}

/// Opens the live database: read-only for a dry run, which then provably
/// changes nothing, and otherwise only after the checks a change needs.
fn open_live(db: &Path, opts: Opts) -> Result<Store> {
    if opts.dry_run {
        return Store::open_existing(db, Access::Read);
    }
    // There is no journal mode to check. `off` and `memory`, the two under
    // which a transaction cannot be rolled back, are settings of the
    // connection that asks for them and are never stored in the file, so
    // this connection always has the default rollback journal, or WAL,
    // which is stored and can roll back.
    check_owner(db)?;
    Store::open_existing(db, Access::Write)
}

const LIVE: &str = "The live database";
const BACKUP: &str = "The backup";

/// Refuses a key that is not stored exactly as given, suggesting keys that
/// look like it. Suggesting is as far as it goes: nothing here ever acts on
/// a key it was not given character for character.
fn require_key(store: &Store, key: &str, what: &str) -> Result<()> {
    if !store.rows(key)?.is_empty() {
        return Ok(());
    }
    let near = similar_keys(&store.summaries()?, key);
    let hint = if near.is_empty() && what == BACKUP {
        "`recall-server admin list --backup <file>` shows every key it holds.".to_string()
    } else if near.is_empty() {
        "`recall-server admin list` shows every key.".to_string()
    } else {
        let near: Vec<String> = near.iter().map(|k| format!("{k:?}")).collect();
        format!("Keys that look similar: {}.", near.join(", "))
    };
    bail!(
        "{what} holds no project key that is exactly {key:?}. Keys are matched exactly, never \
         by prefix or pattern. {hint}"
    )
}

fn similar_keys(summaries: &[Summary], key: &str) -> Vec<String> {
    let needle = key.trim().to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    summaries
        .iter()
        .map(|s| &s.project_key)
        .filter(|k| {
            let k = k.to_lowercase();
            k.contains(&needle) || needle.contains(&k)
        })
        .take(10)
        .cloned()
        .collect()
}

/// The part every change shares: show it, check it, confirm it, back up,
/// check the backup, apply it, and check it held. `again` is the command
/// line that makes the same change, for when that check says to.
fn carry_out(ctx: &mut Ctx, store: &Store, plan: &Plan, again: &str, opts: Opts) -> Result<()> {
    writeln!(ctx.out, "Database: {}", ctx.db.display())?;
    describe(ctx.out, plan)?;
    if let Some(why) = plan.refusal() {
        bail!("refusing: {why}");
    }
    if let Plan::Remove { key, .. } = plan {
        warn_unique(ctx.out, store, key)?;
    }
    if plan.expected_changes() == 0 {
        writeln!(ctx.out, "Nothing to change.")?;
        return Ok(());
    }
    if opts.dry_run {
        writeln!(
            ctx.out,
            "Dry run: nothing was changed, and no backup was taken."
        )?;
        return Ok(());
    }

    // Asked before the confirmation, so nobody types a key only to be told
    // there is nowhere to put the backup.
    let Some(backup_dir) = ctx.backup_dir.clone() else {
        bail!(
            "RECALL_BACKUP_DIR is not set, so there is nowhere to take the backup every change \
             takes first. Set it to where the server's own snapshots go (/backups in the \
             Docker setup)"
        );
    };
    confirm(ctx, plan, opts.yes)?;

    // Its own directory, because the server prunes `recall-*.db` in the
    // backup directory itself down to RECALL_BACKUP_KEEP: a snapshot taken
    // here must neither push out one of those nor be pushed out by them.
    // `usize::MAX` keeps every one, and `backup-offbox.sh` copies them too,
    // since rclone's include pattern matches at any depth.
    let snapshot = store
        .backup(backup_dir.join("admin"), usize::MAX)
        .context("taking the backup that every change takes first")?;
    writeln!(ctx.out, "Backup written: {}", snapshot.display())?;

    let checked = Store::open_existing(&snapshot, Access::Read)
        .and_then(|taken| plan.snapshot_mismatch(&taken));
    match checked {
        Ok(None) => {}
        Ok(Some(why)) => {
            discard(ctx.out, &snapshot);
            bail!(
                "the backup just taken does not hold the rows shown above: {why}. Either a \
                 push arrived since they were shown, or the backup is incomplete. Run the \
                 command again to see them as they are now"
            );
        }
        Err(err) => {
            discard(ctx.out, &snapshot);
            return Err(err.context("reading back the backup just taken"));
        }
    }

    let changed = match store.apply(plan) {
        Ok(changed) => changed,
        Err(err) => {
            // A change abandoned before it began leaves the database as
            // it was, so its backup would only pile up, one per retry, next
            // to the one the retry takes. Any other failure keeps its
            // backup: something was attempted, and the backup is what shows
            // the state it was attempted on.
            if err.is::<Abandoned>() {
                discard(ctx.out, &snapshot);
            }
            return Err(err);
        }
    };

    // Committed. From here, every error must be an AfterCommit, or main
    // would claim the database was not changed.
    let _ = writeln!(ctx.out, "{}", done(plan, changed));
    settle(ctx, store, plan, again)
}

/// Deletes the backup of a change that did not happen, and says so.
fn discard(out: &mut dyn Write, snapshot: &Path) {
    let _ = match std::fs::remove_file(snapshot) {
        Ok(()) => writeln!(
            out,
            "Deleted that backup again: the change it was taken for did not happen."
        ),
        Err(err) => writeln!(
            out,
            "The change that backup was taken for did not happen, and deleting it failed \
             ({err}); delete it by hand."
        ),
    };
}

/// Warns, before the confirmation, that removing `key` removes content
/// that is under no other key.
///
/// The case it is for: a key being folded into another, where the owner
/// believes its notes already arrived there. If they did, this is silent.
fn warn_unique(out: &mut dyn Write, store: &Store, key: &str) -> Result<()> {
    let unique = store.unique_live_paths(key)?;
    if unique.is_empty() {
        return Ok(());
    }
    writeln!(
        out,
        "Warning: {} of these file(s) hold content that no live file under any other key has, \
         so after this it exists only in the backup this command takes:",
        unique.len()
    )?;
    for path in &unique {
        writeln!(out, "  {}", shown(path))?;
    }
    writeln!(
        out,
        "If {key:?} is being folded into another key, rename it to an archive key such as {:?} \
         instead, and bring the content across by hand (\"Folding one key into another\" in \
         deploy/README.md).",
        archive_key(key)
    )?;
    Ok(())
}

/// Waits until a push that was in flight when `plan` committed has had time
/// to land, then checks the change still stands.
///
/// Only a change that replaced or emptied existing rows needs it: a push in
/// flight read one of those rows, and when it writes, it writes the old key
/// or a merge of the old row. A restore that only added rows replaced
/// nothing a push could have read.
fn settle(ctx: &mut Ctx, store: &Store, plan: &Plan, again: &str) -> Result<()> {
    let checks = match plan {
        Plan::Rename { .. } | Plan::Remove { .. } => true,
        Plan::Restore(r) => r.replaces_live_rows(),
    };
    if !checks {
        return Ok(());
    }
    let _ = writeln!(
        ctx.out,
        "Waiting {:.1}s before checking that the change held. The server reads a file before \
         merging a push into it and writes the result up to RECALL_MERGE_TIMEOUT_MS later, \
         holding no lock in between, so a push already being merged as this committed can land \
         after it and partly undo it. The change is committed; interrupting now only skips the \
         check.",
        ctx.settle.as_secs_f64()
    );
    let _ = ctx.out.flush();
    std::thread::sleep(ctx.settle);

    let undone = plan.undone(store).map_err(|err| {
        AfterCommit(format!(
            "the change was committed, but checking afterwards that it held failed: {err:#}. \
             Look with `recall-server admin list`"
        ))
    })?;
    if undone.is_empty() {
        let _ = writeln!(ctx.out, "Checked: the change held.");
        return Ok(());
    }
    let paths: Vec<String> = undone.iter().map(|p| format!("  {}", shown(p))).collect();
    let paths = paths.join("\n");
    let n = undone.len();
    Err(AfterCommit(match plan {
        Plan::Rename { from, to, .. } => format!(
            "the rename was committed, but {n} row(s) are under {from:?} again:\n{paths}\n\
             Either a push already being merged when it committed landed afterwards, or a \
             machine is still syncing under {from:?}. Those rows are newer than what moved to \
             {to:?}, and nothing is lost: they are in the live database. Set \
             RECALL_PROJECT_KEY={to} on any machine still using {from:?}. Then the rename \
             cannot simply be run again, since {to:?} now holds rows: rename {from:?} to an \
             archive key such as {:?} and bring what those rows add into {to:?} by hand, as \
             \"Folding one key into another\" in deploy/README.md describes.",
            archive_key(from)
        ),
        Plan::Remove { key, .. } => format!(
            "the remove was committed, but {n} row(s) are under {key:?} again:\n{paths}\n\
             Either a push already being merged when it committed landed afterwards, or a \
             machine is still syncing under {key:?}. Set RECALL_PROJECT_KEY on any machine \
             still using {key:?}, then run `{again}` again; it shows these rows first and takes \
             a backup of them."
        ),
        Plan::Restore(r) => format!(
            "the restore was committed, but {n} restored row(s) under {:?} are no longer what \
             it wrote:\n{paths}\nEither a push already being merged when it committed wrote \
             over them (a merge made from the version the restore replaced), or a machine has \
             edited them since. To put the backup's version back, run `{again}` again; it \
             shows each difference first.",
            r.key
        ),
    })
    .into())
}

/// Asks the owner to type back every key the change names: a rename's
/// target as well as its source, since a typo there strands the project
/// where no machine will ever pull it.
fn confirm(ctx: &mut Ctx, plan: &Plan, yes: bool) -> Result<()> {
    let keys: Vec<(&str, &str)> = match plan {
        Plan::Rename { from, to, .. } => vec![
            ("the project key to rename", from),
            ("the key to rename it to", to),
        ],
        Plan::Remove { key, .. } => vec![("the project key", key)],
        Plan::Restore(r) => vec![("the project key", &r.key)],
    };
    if yes {
        let named: Vec<String> = keys
            .iter()
            .map(|(what, key)| format!("{} {key:?}", what.trim_start_matches("the ")))
            .collect();
        writeln!(
            ctx.out,
            "Confirmed with --yes for {}.",
            named.join(", and ")
        )?;
        return Ok(());
    }
    for (i, (what, key)) in keys.iter().enumerate() {
        let lead = if i == 0 {
            "To go ahead, type"
        } else {
            "Then type"
        };
        write!(
            ctx.out,
            "{lead} {what}, {key:?}, exactly, without the quotes: "
        )?;
        ctx.out.flush()?;
        let mut line = String::new();
        if ctx
            .input
            .read_line(&mut line)
            .context("reading the confirmation")?
            == 0
        {
            bail!("nothing was typed to confirm. Pass --yes to confirm without a prompt");
        }
        if !ctx.terminal {
            writeln!(ctx.out)?;
        }
        let typed = line.strip_suffix('\n').unwrap_or(&line);
        let typed = typed.strip_suffix('\r').unwrap_or(typed);
        if typed != *key {
            bail!("{typed:?} is not {key:?}, so the change was not confirmed");
        }
    }
    Ok(())
}

fn describe(out: &mut dyn Write, plan: &Plan) -> io::Result<()> {
    match plan {
        Plan::Rename {
            from,
            to,
            rows,
            occupied,
        } => {
            writeln!(
                out,
                "Rename {from:?} to {to:?}: {} row(s) move ({}).",
                rows.len(),
                counts(rows)
            )?;
            for r in rows {
                writeln!(out, "  {}", path_line(r))?;
            }
            if !occupied.is_empty() {
                writeln!(
                    out,
                    "{to:?} already holds {} row(s) ({}):",
                    occupied.len(),
                    counts(occupied)
                )?;
                for r in occupied {
                    writeln!(out, "  {}", path_line(r))?;
                }
            }
        }
        Plan::Remove { key, rows } => {
            writeln!(
                out,
                "Remove {key:?}: {} row(s) are deleted, content included ({}).",
                rows.len(),
                counts(rows)
            )?;
            for r in rows {
                writeln!(out, "  {}", path_line(r))?;
            }
        }
        Plan::Restore(r) => describe_restore(out, r)?,
    }
    Ok(())
}

fn describe_restore(out: &mut dyn Write, r: &Restore) -> io::Result<()> {
    let deleting = r.applied_deletions().len();
    writeln!(
        out,
        "Restore {:?} from the backup: {} row(s) would change, {} added and {} overwritten{}.",
        r.key,
        r.add.len() + r.overwrite.len() + deleting,
        r.add.len(),
        r.overwrite.len(),
        if deleting > 0 {
            format!(", and {deleting} live file(s) deleted")
        } else {
            String::new()
        }
    )?;
    let width = r
        .backup
        .iter()
        .map(|b| shown(&b.file_path).chars().count())
        .chain(r.live_only.iter().map(|p| shown(p).chars().count()))
        .max()
        .unwrap_or(0);
    for row in &r.add {
        writeln!(
            out,
            "  add        {:<width$}  {}, {} bytes, updated {}, source {}",
            shown(&row.file_path),
            state(row),
            row.content.len(),
            row.updated_at,
            source(row)
        )?;
    }
    for (live, backup) in &r.overwrite {
        writeln!(
            out,
            "  overwrite  {:<width$}  {}",
            shown(&live.file_path),
            differences(live, backup)
        )?;
    }
    for (live, backup) in r.applied_deletions() {
        writeln!(
            out,
            "  delete     {:<width$}  {}; deleted from every machine at its next pull",
            shown(&live.file_path),
            differences(live, backup)
        )?;
    }
    for (live, _) in r.skipped_deletions() {
        writeln!(
            out,
            "  skipped    {:<width$}  a file now, deleted in the backup; left as it is \
             (--overwrite --restore-deletions would delete it on every machine)",
            shown(&live.file_path)
        )?;
    }
    for path in &r.unchanged {
        writeln!(out, "  unchanged  {}", shown(path))?;
    }
    for path in &r.live_only {
        writeln!(
            out,
            "  untouched  {:<width$}  not in the backup, so it stays as it is",
            shown(path)
        )?;
    }
    Ok(())
}

/// Every field an overwrite would change, live value first.
fn differences(live: &Row, backup: &Row) -> String {
    let mut parts = Vec::new();
    if live.deleted != backup.deleted {
        parts.push(format!("{} -> {}", state(live), state(backup)));
    }
    if live.content != backup.content {
        parts.push(if live.content.len() == backup.content.len() {
            format!("content differs, {} bytes each", live.content.len())
        } else {
            format!(
                "content {} -> {} bytes",
                live.content.len(),
                backup.content.len()
            )
        });
    }
    if live.updated_at != backup.updated_at {
        parts.push(format!(
            "updated {} -> {}",
            live.updated_at, backup.updated_at
        ));
    }
    if live.source_env != backup.source_env {
        parts.push(format!("source {} -> {}", source(live), source(backup)));
    }
    parts.join("; ")
}

fn done(plan: &Plan, changed: usize) -> String {
    match plan {
        Plan::Rename { from, to, .. } => format!(
            "Done: moved {changed} row(s) from {from:?} to {to:?}. A machine still syncing under \
             {from:?} will write to it again on its next push; set RECALL_PROJECT_KEY={to} there."
        ),
        Plan::Remove { key, .. } => format!(
            "Done: removed {changed} row(s) under {key:?}. A machine still syncing under it \
             will create it again on its next push."
        ),
        Plan::Restore(r) => {
            let mut said = format!(
                "Done: restored {changed} row(s) under {:?} ({} added, {} overwritten",
                r.key,
                r.add.len(),
                r.overwrite.len()
            );
            let deleted = r.applied_deletions().len();
            if deleted > 0 {
                said += &format!(
                    ", {deleted} live file(s) turned into tombstones, which deletes them on \
                     every machine at its next pull"
                );
            }
            said += ").";
            let skipped = r.skipped_deletions().len();
            if skipped > 0 {
                said += &format!(
                    " {skipped} live file(s) the backup has as deleted were left as they are."
                );
            }
            said + " A machine that edits a restored file before it next pulls pushes its own \
                    version over the restored one."
        }
    }
}

fn counts(rows: &[Row]) -> String {
    let tombstones = rows.iter().filter(|r| r.deleted).count();
    format!(
        "{} file(s), {tombstones} tombstone(s)",
        rows.len() - tombstones
    )
}

fn path_line(row: &Row) -> String {
    if row.deleted {
        format!("{}  (tombstone)", shown(&row.file_path))
    } else {
        shown(&row.file_path).into_owned()
    }
}

fn state(row: &Row) -> &'static str {
    if row.deleted {
        "tombstone"
    } else {
        "file"
    }
}

/// The machine a row came from. NULL and `''` are told apart, because a
/// restore compares them and would otherwise report a change it cannot show.
fn source(row: &Row) -> &str {
    match row.source_env.as_deref() {
        None => "(none)",
        Some("") => "\"\"",
        Some(s) => s,
    }
}

/// A key or path as printed: as is, unless it would be ambiguous on a
/// terminal, in which case it is quoted and escaped, so the exact characters
/// are always visible.
///
/// Ambiguous means empty, starting with a dash, holding whitespace, or
/// holding anything `{:?}` would escape: a control character, a quote or
/// backslash, or one of the characters Rust treats as unprintable, which
/// include zero-width and bidi format characters. On top of those, anything
/// [`invisible`] names, since a few (fillers, variation selectors) are
/// printable to Rust and print as nothing on a terminal all the same.
fn shown(s: &str) -> Cow<'_, str> {
    // An apostrophe is left alone: inside double quotes it is unambiguous,
    // and `char::escape_debug` would escape it where `{:?}` on a string
    // does not.
    let escaped = |c: char| c != '\'' && c.escape_debug().ne(std::iter::once(c));
    if s.is_empty()
        || s.starts_with('-')
        || s.chars()
            .any(|c| c.is_whitespace() || escaped(c) || invisible(c))
    {
        let mut quoted = String::from("\"");
        for c in s.chars() {
            if escaped(c) {
                quoted.extend(c.escape_debug());
            } else if invisible(c) {
                quoted += &format!("\\u{{{:x}}}", c as u32);
            } else {
                quoted.push(c);
            }
        }
        quoted.push('"');
        Cow::Owned(quoted)
    } else {
        Cow::Borrowed(s)
    }
}

/// Refuses to change the database as a user other than its owner.
///
/// `docker exec` runs as root unless told otherwise, while the server runs
/// as `node`. A change made as root leaves root-owned files behind: a
/// backup, and after a crash mid-transaction a hot journal, which the
/// server cannot open to roll back, so it cannot read the database until
/// something fixes the ownership.
#[cfg(target_os = "linux")]
fn check_owner(db: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    // A database that is not there is `open_existing`'s to report, with the
    // message written for exactly that.
    let Ok(file) = std::fs::metadata(db) else {
        return Ok(());
    };
    // /proc/self is owned by the process's effective uid: the one way to
    // learn it from std alone, without a libc dependency for one call.
    let me = std::fs::metadata("/proc/self").ok().map(|m| m.uid());
    match owner_refusal(me, file.uid(), db) {
        Some(why) => bail!("{why}"),
        None => Ok(()),
    }
}

/// Why a process running as `me` must not change the database at `db`,
/// owned by `owner`, if it must not. `None` for `me` is a process that
/// could not tell who it runs as.
///
/// That case refuses rather than letting the change through. The check
/// exists for the one mistake that is easy to make (forgetting `-u node`)
/// and costly to find out about (a server that cannot open its database
/// after a crash); a check that passes whenever it cannot look would pass
/// in the very containers most likely to be unusual. A container without
/// /proc is rare enough that refusing there costs little, and the message
/// says what is wrong.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn owner_refusal(me: Option<u32>, owner: u32, db: &Path) -> Option<String> {
    let run_as = "Run it as the database's owner, which in the Docker setup is: \
                  docker exec -it -u node recall-server recall-server admin ...";
    match me {
        None => Some(format!(
            "cannot tell which user this runs as (/proc/self cannot be read), so cannot check \
             that it is {}'s owner, uid {owner}. A change made as another user can leave files \
             the server cannot open. {run_as}",
            db.display()
        )),
        Some(me) if me != owner => Some(format!(
            "this runs as uid {me} but {} belongs to uid {owner}. A change made as another user \
             can leave files the server cannot open. {run_as}",
            db.display()
        )),
        Some(_) => None,
    }
}

#[cfg(not(target_os = "linux"))]
fn check_owner(_db: &Path) -> Result<()> {
    Ok(())
}

/// Whether two paths are the same file, hard links and symlinks included.
fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(x), Ok(y)) = (std::fs::metadata(a), std::fs::metadata(b)) {
            return x.dev() == y.dev() && x.ino() == y.ino();
        }
    }
    matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_every_command() {
        assert_eq!(
            parse(&args(&["rename", "a", "b", "--dry-run"])),
            Ok(Command::Rename {
                from: "a".into(),
                to: "b".into(),
                opts: Opts {
                    dry_run: true,
                    yes: false
                }
            })
        );
        assert_eq!(
            parse(&args(&["restore", "--yes", "f.db", "k", "--overwrite"])),
            Ok(Command::Restore {
                backup: "f.db".into(),
                key: "k".into(),
                overwrite: true,
                deletions: false,
                opts: Opts {
                    dry_run: false,
                    yes: true
                }
            })
        );
        assert_eq!(
            parse(&args(&["list", "--backup=b.db", "k"])),
            Ok(Command::List {
                backup: Some("b.db".into()),
                key: Some("k".into())
            })
        );
        assert_eq!(parse(&args(&["remove", "--help"])), Ok(Command::Help));
    }

    /// `--` is how a key that starts with a dash is named, and after it
    /// nothing is an option, `--yes` included.
    #[test]
    fn everything_after_a_double_dash_is_a_key() {
        assert_eq!(
            parse(&args(&["remove", "--", "-odd"])),
            Ok(Command::Remove {
                key: "-odd".into(),
                opts: Opts::default()
            })
        );
        assert!(parse(&args(&["remove", "k", "--", "--yes"])).is_err());
    }

    #[test]
    fn refuses_what_it_does_not_understand() {
        for bad in [
            &[][..],
            &["serve"],
            &["remove"],
            &["remove", "a", "b"],
            &["rename", "a"],
            &["remove", "k", "--force"],
            &["remove", "k", "--overwrite"],
            &["list", "--yes"],
            &["list", "--backup"],
            &["restore", "k"],
        ] {
            assert!(
                parse(&args(bad)).is_err(),
                "{bad:?} should be a usage error"
            );
        }
    }

    #[test]
    fn ambiguous_names_are_printed_quoted() {
        assert_eq!(shown("acme/app"), "acme/app");
        assert_eq!(shown("local:-Users-me-thing"), "local:-Users-me-thing");
        assert_eq!(shown("me/thing "), "\"me/thing \"");
        assert_eq!(shown(""), "\"\"");
        assert_eq!(shown("-x"), "\"-x\"");
        assert_eq!(shown("a\u{7}b"), "\"a\\u{7}b\"");
        assert_eq!(shown("it's"), "it's");
        assert_eq!(shown("a\"b"), "\"a\\\"b\"");
    }

    /// A key that differs from another only by a character that prints as
    /// nothing must not print the same as it.
    #[test]
    fn invisible_characters_are_printed_escaped() {
        for (raw, printed) in [
            ("me/\u{200B}thing", "\"me/\\u{200b}thing\""),
            ("\u{FEFF}me/thing", "\"\\u{feff}me/thing\""),
            ("me/thing\u{2060}", "\"me/thing\\u{2060}\""),
            ("me/\u{202E}thing", "\"me/\\u{202e}thing\""),
            ("me/\u{00AD}thing", "\"me/\\u{ad}thing\""),
            // Printable to Rust, invisible on a terminal all the same.
            ("me/\u{3164}thing", "\"me/\\u{3164}thing\""),
            ("me/thing\u{FE0F}", "\"me/thing\\u{fe0f}\""),
        ] {
            assert_eq!(shown(raw), printed, "{raw:?}");
            assert_ne!(shown(raw), shown("me/thing"));
        }
    }

    #[test]
    fn restore_deletions_comes_only_with_overwrite() {
        assert!(parse(&args(&["restore", "f.db", "k", "--restore-deletions"])).is_err());
        assert!(parse(&args(&["remove", "k", "--restore-deletions"])).is_err());
        assert_eq!(
            parse(&args(&[
                "restore",
                "f.db",
                "k",
                "--overwrite",
                "--restore-deletions"
            ])),
            Ok(Command::Restore {
                backup: "f.db".into(),
                key: "k".into(),
                overwrite: true,
                deletions: true,
                opts: Opts::default()
            })
        );
    }

    /// The decision alone, so it is tested wherever the tests run, not only
    /// as root, which is the one place a test can make a file someone
    /// else's.
    #[test]
    fn only_the_databases_owner_may_change_it() {
        let db = Path::new("/data/recall.db");
        assert_eq!(owner_refusal(Some(1000), 1000, db), None);
        let root = owner_refusal(Some(0), 1000, db).unwrap();
        assert!(root.contains("uid 0"), "{root}");
        assert!(root.contains("uid 1000"), "{root}");
        assert!(root.contains("-u node"), "{root}");
        // Not knowing who this runs as is a refusal, never a pass.
        let blind = owner_refusal(None, 1000, db).unwrap();
        assert!(blind.contains("/proc/self"), "{blind}");
        assert!(blind.contains("-u node"), "{blind}");
        assert!(owner_refusal(None, 0, db).is_some(), "not even for root");
    }

    /// Read the way the server reads it, or the wait would end before the
    /// push it exists for.
    #[test]
    fn the_wait_after_a_change_follows_the_servers_merge_timeout() {
        let with = |pairs: &'static [(&'static str, &'static str)]| {
            settle_window(&move |k: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == k)
                    .map(|(_, v)| v.to_string())
            })
        };
        assert_eq!(with(&[]), Duration::from_millis(45_000) + SETTLE_MARGIN);
        assert_eq!(
            with(&[("RECALL_MERGE_TIMEOUT_MS", "1234")]),
            Duration::from_millis(1234) + SETTLE_MARGIN
        );
        // The server's fallbacks: zero and nonsense both mean 45 seconds.
        for bad in ["0", "soon", "-5"] {
            let pairs: &'static [(&str, &str)] =
                Box::leak(Box::new([("RECALL_MERGE_TIMEOUT_MS", bad)]));
            assert_eq!(with(pairs), Duration::from_millis(45_000) + SETTLE_MARGIN);
        }
        // Only a literal "false" turns merge off, as in the server.
        assert_eq!(with(&[("RECALL_MERGE_ENABLED", "false")]), SETTLE_MARGIN);
        assert_eq!(
            with(&[
                ("RECALL_MERGE_ENABLED", "no"),
                ("RECALL_MERGE_TIMEOUT_MS", "10")
            ]),
            Duration::from_millis(10) + SETTLE_MARGIN
        );
    }

    #[test]
    fn a_command_to_run_again_is_quoted_for_the_shell() {
        assert_eq!(
            sh_args(&["me/thing", "local:-Users-me"]),
            "me/thing local:-Users-me"
        );
        assert_eq!(sh_args(&["my project"]), "'my project'");
        assert_eq!(sh_args(&["it's"]), r"'it'\''s'");
        assert_eq!(sh_args(&["-odd", "x"]), "-- -odd x");
        assert_eq!(sh_args(&[""]), "''");
    }

    #[test]
    fn similar_keys_are_hints_by_substring_either_way() {
        let s = |k: &str| Summary {
            project_key: k.into(),
            files: 1,
            tombstones: 0,
            last_updated_at: String::new(),
        };
        let all = [s("acme/app"), s("acme/api"), s("other")];
        assert_eq!(similar_keys(&all, "ACME"), vec!["acme/app", "acme/api"]);
        assert_eq!(similar_keys(&all, "acme/app-2"), vec!["acme/app"]);
        assert!(similar_keys(&all, " ").is_empty());
    }
}
