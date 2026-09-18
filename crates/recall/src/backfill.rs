//! `recall backfill` — the first sync, for a project whose memory predates
//! Recall.
//!
//! Loud, like `promote` and unlike the hooks: nobody types this by accident,
//! so a file that could not be sent is worth a line of output rather than a
//! silence. It is still not allowed to be destructive — the work of deciding
//! what is safe to send happens in `recall_hooks::backfill`, and what reaches
//! this module is the report.

use recall_hooks::{backfill, exit, Disposition, Entry};

use crate::project;

/// Sends what the server is missing, then says what it left behind and why.
pub async fn run() -> anyhow::Result<i32> {
    let ctx = project::resolve().hook_context()?;

    let outcome = match backfill(&ctx).await {
        Ok(outcome) => outcome,
        Err(err) => {
            eprintln!("recall: {err}");
            // Every error that reaches here happens before anything is sent —
            // the round trip that asks what the server holds, or a local read
            // that precedes the loop — so nothing was sent in either case.
            // The split is the one `promote` draws: the server's failure is
            // not the same as this machine's.
            return Ok(match err {
                recall_hooks::Error::Pull { .. } => exit::SERVER,
                _ => exit::CONFIG,
            });
        }
    };

    // Every disposition is counted. A summary that does not account for every
    // file invites the reader to assume the difference was fine, and the one
    // originally missing was `Deleted` — the category most worth noticing.
    println!(
        "recall: {} sent, {} already on the server, {} held back, {} deleted on the \
         server, {} skipped ({})",
        outcome.count(Disposition::Sent),
        outcome.count(Disposition::Matches),
        outcome.count(Disposition::Held),
        outcome.count(Disposition::Deleted),
        outcome.count(Disposition::Unroutable)
            + outcome.count(Disposition::NotUtf8)
            + outcome.count(Disposition::Refused)
            + outcome.count(Disposition::Ignored)
            + outcome.count(Disposition::Internal),
        ctx.project_key()
    );

    // Each list names files the user can act on. A count alone tells someone
    // that something is wrong without telling them where.
    list(
        &outcome.entries,
        Disposition::Held,
        "held back — the server has a different version of these, so they were \
         not overwritten. Nothing on this machine reconciles them: ask Claude to \
         edit one and the server merges the two versions. Do not simply delete \
         your copy — that propagates as a delete and removes the server's too:",
    );
    list(
        &outcome.entries,
        Disposition::Deleted,
        "deleted on the server — these were removed on another machine and this \
         one has not caught up. 'recall pull' will remove them here:",
    );
    list(
        &outcome.entries,
        Disposition::Refused,
        "refused — the server will not accept these as they are, so the rest of \
         the run went on without them:",
    );
    list(
        &outcome.entries,
        Disposition::Unroutable,
        "not in any scope, so these belong nowhere. Each line says what would \
         change that:",
    );
    list(
        &outcome.entries,
        Disposition::NotUtf8,
        "not text, so they cannot be synced at all:",
    );
    list(
        &outcome.entries,
        Disposition::Ignored,
        "on disk but unreadable:",
    );

    if let Some(reason) = &outcome.baseline {
        eprintln!("\nrecall: {reason}");
    }
    if let Some(reason) = &outcome.stopped {
        eprintln!(
            "\nrecall: stopped {reason}\n  {} file(s) after it were not reached.",
            outcome.not_reached
        );
        return Ok(exit::SERVER);
    }
    Ok(exit::OK)
}

fn list(entries: &[Entry], what: Disposition, heading: &str) {
    let named: Vec<&Entry> = entries.iter().filter(|e| e.disposition == what).collect();
    if named.is_empty() {
        return;
    }
    println!("\n  {heading}");
    for entry in named {
        match &entry.detail {
            Some(why) => println!("    {} — {why}", entry.path),
            None => println!("    {}", entry.path),
        }
    }
}
