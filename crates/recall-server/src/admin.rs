//! `recall-server admin`: listing, renaming, removing and restoring the
//! memory stored under a project key, from a shell on the server's host.
//!
//! ```text
//! docker compose exec -u node recall-server recall-server admin list
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
//! 1. names its key exactly (never a prefix or a pattern) and is confirmed
//!    by typing that key, or by `--yes`, which prints the key instead;
//! 2. takes a fresh backup first, with [`Store::backup`], into an `admin/`
//!    directory the server's rotation never prunes, and prints its path;
//! 3. runs in one transaction, re-checks inside it that nothing changed
//!    since it was shown, and commits only if `changes()` matches;
//! 4. can be previewed with `--dry-run`, which changes nothing and takes no
//!    backup.
//!
//! It is safe beside a running server. Both wait up to 5 seconds on a lock
//! the other holds, and a change's transaction takes the write lock before
//! it re-reads anything, so neither can fail the other half way; the store's
//! admin module has the detail.

use std::borrow::Cow;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Context, Result};

use crate::store::admin::{Access, Plan, Restore, Row, Summary};
use crate::Store;

const USAGE: &str = "\
Changes the memory a Recall server stores, from a shell on its host.

Usage:
  recall-server admin list [--backup <file>] [<key>]
  recall-server admin rename <from> <to> [--dry-run] [--yes]
  recall-server admin remove <key> [--dry-run] [--yes]
  recall-server admin restore <backup-file> <key> [--overwrite] [--dry-run] [--yes]

  list      Every project key with its files, tombstones and last update.
            With a key, that key's files. With --backup, what a backup holds.
  rename    Move every row of one key to another key that holds none.
  remove    Delete every row of one key.
  restore   Copy one key's rows from a backup into the live database.

Options:
  --dry-run    Print exactly what would change, and change nothing.
  --yes        Confirm without typing the key. The key is printed instead.
  --overwrite  Let restore replace live rows that differ from the backup.
  --backup     Make list read a backup file instead of the live database.

Keys are matched exactly, never by prefix or pattern. Put -- before a key
that starts with a dash.

Every change takes a backup first, into $RECALL_BACKUP_DIR/admin/, which the
server's rotation never prunes. It runs in one transaction and commits only
if exactly the rows it showed changed. The database is RECALL_DB_PATH.

In the Docker setup, run it as the user the server runs as:
  docker compose exec -u node recall-server recall-server admin list";

/// Runs `recall-server admin`, given the arguments after `admin`.
///
/// Exits 0 on success, including a dry run and a change with nothing to do;
/// 1 when a command is refused or fails, having changed nothing; 2 for a
/// usage error.
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
        Err(err) => {
            eprintln!("recall-server admin: {err:#}");
            // True of every error a change can return: the only step after
            // COMMIT is printing, and its errors are ignored below.
            if changes {
                eprintln!("The database was not changed.");
            }
            ExitCode::FAILURE
        }
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
        "restore" => &["--overwrite", "--dry-run", "--yes"],
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
            "--dry-run" | "--yes" | "--overwrite" => flags.push(arg.as_str()),
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
            carry_out(&mut ctx, &store, &plan, &from, opts)
        }
        Command::Remove { key, opts } => {
            let store = open_live(&ctx.db, opts)?;
            require_key(&store, &key, LIVE)?;
            let plan = store.plan_remove(&key)?;
            carry_out(&mut ctx, &store, &plan, &key, opts)
        }
        Command::Restore {
            backup,
            key,
            overwrite,
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
            let plan = store.plan_restore(&key, rows, overwrite)?;
            carry_out(&mut ctx, &store, &plan, &key, opts)
        }
    }
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
    check_owner(db)?;
    let store = Store::open_existing(db, Access::Write)?;
    store.check_journal_mode()?;
    Ok(store)
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
/// apply it.
fn carry_out(ctx: &mut Ctx, store: &Store, plan: &Plan, key: &str, opts: Opts) -> Result<()> {
    writeln!(ctx.out, "Database: {}", ctx.db.display())?;
    describe(ctx.out, plan)?;
    if let Some(why) = plan.refusal() {
        bail!("refusing: {why}");
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
    confirm(ctx, key, opts.yes)?;

    // Its own directory, because the server prunes `recall-*.db` in the
    // backup directory itself down to RECALL_BACKUP_KEEP: a snapshot taken
    // here must neither push out one of those nor be pushed out by them.
    // `usize::MAX` keeps every one, and `backup-offbox.sh` copies them too,
    // since rclone's include pattern matches at any depth.
    let snapshot = store
        .backup(backup_dir.join("admin"), usize::MAX)
        .context("taking the backup that every change takes first")?;
    writeln!(ctx.out, "Backup written: {}", snapshot.display())?;

    let changed = store.apply(plan)?;

    // Committed. Nothing below may fail the command now, or main would
    // claim the database was not changed.
    let _ = writeln!(ctx.out, "{}", done(plan, changed));
    Ok(())
}

fn confirm(ctx: &mut Ctx, key: &str, yes: bool) -> Result<()> {
    if yes {
        writeln!(ctx.out, "Confirmed with --yes for project key {key:?}.")?;
        return Ok(());
    }
    write!(
        ctx.out,
        "To go ahead, type the project key {key:?} exactly, without the quotes: "
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
    if typed != key {
        bail!("{typed:?} is not {key:?}, so the change was not confirmed");
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
    writeln!(
        out,
        "Restore {:?} from the backup: {} row(s) would change, {} added and {} overwritten.",
        r.key,
        r.add.len() + r.overwrite.len(),
        r.add.len(),
        r.overwrite.len()
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
        Plan::Restore(r) => format!(
            "Done: restored {changed} row(s) under {:?} ({} added, {} overwritten). A machine \
             holding a newer copy of a restored file may push it back on its next edit.",
            r.key,
            r.add.len(),
            r.overwrite.len()
        ),
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
/// terminal (empty, edged with whitespace, holding a space or a control
/// character, or starting with a dash), in which case it is quoted and
/// escaped, so the exact characters are always visible.
fn shown(s: &str) -> Cow<'_, str> {
    if s.is_empty() || s.starts_with('-') || s.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        Cow::Owned(format!("{s:?}"))
    } else {
        Cow::Borrowed(s)
    }
}

/// Refuses to change the database as a user other than its owner.
///
/// `docker compose exec` runs as root unless told otherwise, while the
/// server runs as `node`. A change made as root leaves root-owned files
/// behind: a backup, and after a crash mid-transaction a hot journal, which
/// the server cannot open to roll back, so it cannot read the database
/// until something fixes the ownership.
#[cfg(target_os = "linux")]
fn check_owner(db: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    // /proc/self is owned by the process's effective uid: the one way to
    // learn it from std alone, without a libc dependency for one call.
    let (Ok(me), Ok(file)) = (std::fs::metadata("/proc/self"), std::fs::metadata(db)) else {
        return Ok(());
    };
    if me.uid() != file.uid() {
        bail!(
            "this runs as uid {} but {} belongs to uid {}. A change made as another user can \
             leave files the server cannot open. Run it as the database's owner, which in the \
             Docker setup is: docker compose exec -u node recall-server recall-server admin ...",
            me.uid(),
            db.display(),
            file.uid()
        );
    }
    Ok(())
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
