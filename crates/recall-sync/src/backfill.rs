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
            // Nothing was sent: the failure is the round trip that asks what
            // the server already holds, and without that answer sending
            // anything would risk overwriting it.
            return Ok(exit::SERVER);
        }
    };

    println!(
        "recall: {} sent, {} already on the server, {} held back, {} skipped ({})",
        outcome.count(&Disposition::Sent),
        outcome.count(&Disposition::Matches),
        outcome.count(&Disposition::Held),
        outcome.count(&Disposition::Unroutable) + outcome.count(&Disposition::NotUtf8),
        ctx.project_key()
    );

    // Each list names files the user can act on. A count alone tells someone
    // that something is wrong without telling them where.
    list(
        &outcome.entries,
        &Disposition::Held,
        "held back — the server has a different version of these, so they were \
         not overwritten. Run 'recall pull' and reconcile, or edit the file so \
         the push hook sends it:",
    );
    list(
        &outcome.entries,
        &Disposition::Deleted,
        "deleted on the server — these were removed on another machine and this \
         one has not caught up. 'recall pull' will remove them here:",
    );
    list(
        &outcome.entries,
        &Disposition::Unroutable,
        "not in any scope — global sync is off, so these belong nowhere. Set \
         RECALL_GLOBAL_KEY to sync them:",
    );
    list(
        &outcome.entries,
        &Disposition::NotUtf8,
        "not text, so they cannot be synced at all:",
    );

    if let Some(reason) = &outcome.stopped {
        eprintln!("\nrecall: stopped early — {reason}");
        return Ok(exit::SERVER);
    }
    Ok(exit::OK)
}

fn list(entries: &[Entry], what: &Disposition, heading: &str) {
    let named: Vec<&str> = entries
        .iter()
        .filter(|e| &e.disposition == what)
        .map(|e| e.path.as_str())
        .collect();
    if named.is_empty() {
        return;
    }
    println!("\n  {heading}");
    for path in named {
        println!("    {path}");
    }
}
