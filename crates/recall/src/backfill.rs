//! `recall backfill` — the first sync, for a project whose memory predates
//! Recall.
//!
//! Loud, like `promote` and unlike the hooks: nobody types this by accident,
//! so a file that could not be sent is worth a line of output rather than a
//! silence. It is still not allowed to be destructive — the work of deciding
//! what is safe to send happens in `recall_hooks::backfill`, and what reaches
//! this module is the report.

use recall_hooks::{backfill, exit, BackfillOutcome as Outcome, Disposition, Entry};

use crate::project;
use crate::ui::{self, Tone};

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

    ui::title("recall backfill", ctx.project_key());
    print_groups(&outcome);

    if let Some(reason) = &outcome.stopped {
        anstream::eprintln!();
        anstream::eprintln!(
            "{} {}",
            ui::toned(Tone::Bad, Tone::Bad.mark()),
            ui::bold(&format!(
                "Stopped {reason}. {} after it were not reached.",
                files(outcome.not_reached)
            ))
        );
        anstream::eprintln!(
            "  {}",
            ui::accent("→ recall backfill again: it sends only what the server is still missing")
        );
        return Ok(exit::SERVER);
    }
    if let Some(reason) = &outcome.baseline {
        anstream::eprintln!();
        anstream::eprintln!("{} {reason}", ui::toned(Tone::Warn, Tone::Warn.mark()));
    }
    let (tone, summary) = summary(&outcome);
    ui::verdict(tone, &summary);
    Ok(exit::OK)
}

/// What became of the files, one group per line, the ones to act on named.
///
/// A group with nothing in it is not printed: a column of zeros reads as
/// "something happened" when nothing did. Every file is still accounted
/// for, in these lines and the summary together, because a summary that
/// leaves some out invites the reader to assume the difference was fine —
/// and the category that was once missing, `Deleted`, is the one most worth
/// noticing.
fn print_groups(outcome: &Outcome) {
    let n = |what| outcome.count(what);
    let (sent, matches) = (n(Disposition::Sent), n(Disposition::Matches));
    // Everything went, or was already there: the summary says it alone.
    if left(outcome) == 0 && n(Disposition::Internal) == 0 {
        return;
    }
    anstream::println!();
    if sent > 0 {
        ui::step(Tone::Good, &format!("Sent {}", files(sent)), None);
    }
    if matches > 0 {
        ui::step(
            Tone::Good,
            &format!("{} already on the server", files(matches)),
            None,
        );
    }
    // Each group names the files, because a count alone tells someone that
    // something is wrong without telling them where.
    group(
        &outcome.entries,
        Disposition::Held,
        Tone::Warn,
        "held back, as the server has different content and a backfill never overwrites",
        Some(
            "edit one through Claude and the server merges the two; deleting your copy \
             deletes the server's too",
        ),
    );
    group(
        &outcome.entries,
        Disposition::Deleted,
        Tone::Warn,
        "deleted on the server, on another machine",
        Some("recall pull catches this machine up"),
    );
    group(
        &outcome.entries,
        Disposition::Refused,
        Tone::Bad,
        "refused by the server, so skipped",
        Some("fix what each one says, then recall backfill again"),
    );
    group(
        &outcome.entries,
        Disposition::Unroutable,
        Tone::Warn,
        "in no scope, so with nowhere to go",
        Some("do what each one says, then recall backfill again"),
    );
    group(
        &outcome.entries,
        Disposition::Ignored,
        Tone::Bad,
        "on disk but unreadable",
        Some("fix what each one says, then recall backfill again"),
    );
    group(
        &outcome.entries,
        Disposition::NotUtf8,
        Tone::Quiet,
        "not text, which cannot be synced",
        None,
    );
    let internal = n(Disposition::Internal);
    if internal > 0 {
        ui::step(
            Tone::Quiet,
            &format!(
                "{} of Recall's own, half-written, left alone",
                files(internal)
            ),
            None,
        );
    }
}

/// One group of files that were not sent: how many and why, each file with
/// its own reason when it has one, and what to do about them.
fn group(entries: &[Entry], what: Disposition, tone: Tone, why: &str, next: Option<&str>) {
    let named: Vec<&Entry> = entries.iter().filter(|e| e.disposition == what).collect();
    if named.is_empty() {
        return;
    }
    ui::step(tone, &format!("{} {why}", files(named.len())), None);
    for entry in named {
        let line = match &entry.detail {
            Some(detail) => format!("{}: {detail}", entry.path),
            None => entry.path.clone(),
        };
        anstream::println!("      {}", ui::dim(&line));
    }
    if let Some(next) = next {
        anstream::println!("    {}", ui::accent(&format!("→ {next}")));
    }
}

/// How many files were neither sent nor already there, Recall's own
/// half-written ones aside.
fn left(outcome: &Outcome) -> usize {
    let n = |what| outcome.count(what);
    outcome.entries.len()
        - n(Disposition::Sent)
        - n(Disposition::Matches)
        - n(Disposition::Internal)
}

/// The closing line: one sentence for the whole run, with a zero said only
/// when it is the answer.
fn summary(outcome: &Outcome) -> (Tone, String) {
    let sent = outcome.count(Disposition::Sent);
    let matches = outcome.count(Disposition::Matches);
    match (sent, matches, left(outcome)) {
        (0, 0, 0) => (
            Tone::Quiet,
            "Nothing to send: there are no memory files here yet.".to_string(),
        ),
        (0, m, 0) => (
            Tone::Good,
            format!(
                "Nothing to send: the server already has {}.",
                match m {
                    1 => "the one file".to_string(),
                    m => format!("all {m} files"),
                }
            ),
        ),
        (1, 0, 0) => (Tone::Good, "Sent the one file here.".to_string()),
        (s, 0, 0) => (Tone::Good, format!("Sent all {s} files here.")),
        (s, m, 0) => (
            Tone::Good,
            format!("Sent {}. The server has all {} now.", files(s), s + m),
        ),
        (0, _, l) => (
            Tone::Warn,
            format!("Nothing sent. {} left behind, listed above.", files(l)),
        ),
        (s, _, l) => (
            Tone::Warn,
            format!("Sent {}. {} left behind, listed above.", files(s), files(l)),
        ),
    }
}

/// `1 file`, `3 files`.
fn files(n: usize) -> String {
    match n {
        1 => "1 file".to_string(),
        n => format!("{n} files"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(of: &[Disposition]) -> Outcome {
        Outcome {
            entries: of
                .iter()
                .enumerate()
                .map(|(i, d)| Entry {
                    path: format!("{i}.md"),
                    disposition: *d,
                    detail: None,
                })
                .collect(),
            ..Default::default()
        }
    }

    /// A zero is said only when it is the answer, and one line says what
    /// the whole run came to. Mutation: print every count, as it used to.
    #[test]
    fn the_summary_says_zero_only_when_zero_is_the_answer() {
        use Disposition::*;
        let said = |of: &[Disposition]| summary(&outcome(of)).1;
        assert_eq!(
            said(&[]),
            "Nothing to send: there are no memory files here yet."
        );
        assert_eq!(
            said(&[Matches, Matches, Matches]),
            "Nothing to send: the server already has all 3 files."
        );
        assert_eq!(said(&[Sent, Sent]), "Sent all 2 files here.");
        assert_eq!(
            said(&[Sent, Matches]),
            "Sent 1 file. The server has all 2 now."
        );
        assert_eq!(
            said(&[Sent, Held, Deleted, Internal]),
            "Sent 1 file. 2 files left behind, listed above."
        );
        for s in [said(&[Matches]), said(&[Sent, Held])] {
            assert!(!s.contains(" 0 "), "{s}");
        }
        assert_eq!(summary(&outcome(&[Sent, Held])).0, Tone::Warn);
    }
}
