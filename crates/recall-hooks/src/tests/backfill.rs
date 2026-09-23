//! The first sync, which is the one operation allowed to walk the whole
//! memory directory.
//!
//! These are safety tests more than feature tests. `POST /sync` overwrites
//! in place with no timestamp comparison, so the interesting assertions are
//! all about what a backfill *does not* send: a file the server holds
//! differently, a file it has tombstoned, a global note on a machine with
//! global sync off. Each of those, sent, would destroy something.

use recall_wire::File;

use super::{write, Fixture};
use crate::{backfill, Disposition};

/// A file the server holds, for the fake server to serve.
fn held(path: &str, content: &str) -> File {
    File {
        file_path: path.into(),
        content: Some(content.into()),
        source_env: "other-machine".into(),
        updated_at: "2026-01-01T00:00:00.000Z".into(),
        deleted: false,
    }
}

fn disposition_of(outcome: &backfill::Outcome, path: &str) -> Disposition {
    outcome
        .entries
        .iter()
        .find(|e| e.path == path)
        .unwrap_or_else(|| panic!("{path} was not considered at all: {:?}", outcome.entries))
        .disposition
}

/// The whole point: a memory directory that existed before Recall did.
/// Without this, those files reach the server only if Claude happens to edit
/// each one, and a `touch` from the shell never triggers anything at all.
#[tokio::test]
async fn memory_that_predates_recall_reaches_the_server() {
    let fx = Fixture::new().await;
    write(&fx.memory("MEMORY.md"), "# Memory\n");
    write(&fx.memory("topics/auth.md"), "bearer tokens\n");
    write(&fx.memory("topics/deep/ports.md"), "8787\n");

    let out = backfill(&fx.ctx).await.unwrap();

    assert_eq!(out.count(Disposition::Sent), 3);
    assert!(out.stopped.is_none());

    let mut sent: Vec<String> = fx
        .server
        .pushes()
        .into_iter()
        .map(|p| p.file_path)
        .collect();
    sent.sort();
    assert_eq!(
        sent,
        ["MEMORY.md", "topics/auth.md", "topics/deep/ports.md"]
    );
    assert!(
        fx.server
            .pushes()
            .iter()
            .all(|p| p.project_key == "acme/app"),
        "every file belongs to the project scope here"
    );
}

/// The destructive case, and the reason this command asks before it sends.
/// The server's copy is whatever another machine pushed last; `POST /sync`
/// compares no timestamps and returns no conflict, so sending this one would
/// silently overwrite work that only exists there.
#[tokio::test]
async fn a_file_the_server_holds_differently_is_left_alone() {
    let fx = Fixture::new().await;
    fx.server
        .set_files(vec![held("topics/auth.md", "the newer version\n")]);
    write(&fx.memory("topics/auth.md"), "my older local copy\n");

    let out = backfill(&fx.ctx).await.unwrap();

    assert_eq!(disposition_of(&out, "topics/auth.md"), Disposition::Held);
    assert!(
        fx.server.pushes().is_empty(),
        "a backfill that overwrites the server is not a backfill, it is a delete"
    );
}

/// Re-running has to be free. It is the documented way to finish a run that
/// stopped at the rate limit, so a second pass must not re-send everything
/// and burn the budget again.
#[tokio::test]
async fn a_file_the_server_already_has_is_not_sent_twice() {
    let fx = Fixture::new().await;
    fx.server
        .set_files(vec![held("topics/auth.md", "identical bytes\n")]);
    write(&fx.memory("topics/auth.md"), "identical bytes\n");

    let out = backfill(&fx.ctx).await.unwrap();

    assert_eq!(disposition_of(&out, "topics/auth.md"), Disposition::Matches);
    assert!(fx.server.pushes().is_empty());
}

/// A tombstone means someone deleted this on another machine. This disk has
/// simply not caught up yet — `pull` is what removes it here. Sending it
/// would undo a deliberate delete, and the user would have no idea why the
/// file came back.
#[tokio::test]
async fn a_file_the_server_tombstoned_is_not_resurrected() {
    let fx = Fixture::new().await;
    fx.server.set_files(vec![File {
        file_path: "topics/old.md".into(),
        // Carrying content *and* the flag on purpose: with `content: None`
        // this test passes even if the `deleted` check is deleted, because
        // there is nothing to compare against either way.
        content: Some("what it said before it was removed\n".into()),
        source_env: "other-machine".into(),
        updated_at: "2026-01-01T00:00:00.000Z".into(),
        deleted: true,
    }]);
    write(&fx.memory("topics/old.md"), "deleted elsewhere\n");

    let out = backfill(&fx.ctx).await.unwrap();

    assert_eq!(disposition_of(&out, "topics/old.md"), Disposition::Deleted);
    assert!(fx.server.pushes().is_empty());
}

/// With global sync off, `global/` belongs to no scope. The project scope
/// matches everything *else*, so the failure mode of a naive walk is not a
/// crash — it is someone's personal notes quietly filed into one
/// repository's history, visible to everyone who pulls that project.
#[tokio::test]
async fn global_notes_are_never_swept_into_the_project() {
    let fx = Fixture::new().await;
    write(&fx.memory("global/editor.md"), "I use helix\n");
    write(&fx.memory("topics/auth.md"), "bearer tokens\n");

    let out = backfill(&fx.ctx).await.unwrap();

    assert_eq!(
        disposition_of(&out, "global/editor.md"),
        Disposition::Unroutable
    );
    let sent: Vec<String> = fx
        .server
        .pushes()
        .into_iter()
        .map(|p| p.file_path)
        .collect();
    assert_eq!(
        sent,
        ["topics/auth.md"],
        "the project's own file goes, the global note does not"
    );
}

/// And with global sync on it goes to the global key, not the project's —
/// under its scope-relative path, the same as a push.
#[tokio::test]
async fn with_global_sync_on_a_global_note_goes_to_the_global_key() {
    let fx = Fixture::with_global_scope().await;
    write(&fx.memory("global/editor.md"), "I use helix\n");

    backfill(&fx.ctx).await.unwrap();

    let pushes = fx.server.pushes();
    let note = pushes
        .iter()
        .find(|p| p.file_path == "editor.md")
        .expect("the global note should be sent under its scope-relative path");
    assert_eq!(note.project_key, "global:eko");
}

/// A `.DS_Store`, a pasted screenshot, anything binary that happens to sit
/// in the memory directory. One of them must not end the run — it is not the
/// user's fault and there is nothing to fix.
#[tokio::test]
async fn a_file_that_is_not_text_is_skipped_without_ending_the_run() {
    let fx = Fixture::new().await;
    std::fs::create_dir_all(fx.ctx.memory_dir.join("topics")).unwrap();
    std::fs::write(fx.memory("topics/diagram.png"), [0xff, 0xd8, 0xff, 0xe0]).unwrap();
    write(&fx.memory("topics/auth.md"), "bearer tokens\n");

    let out = backfill(&fx.ctx).await.unwrap();

    assert_eq!(
        disposition_of(&out, "topics/diagram.png"),
        Disposition::NotUtf8
    );
    assert_eq!(out.count(Disposition::Sent), 1);
    assert!(out.stopped.is_none());
}

/// The rate limit is the expected way for this to end on a large directory:
/// 60 requests a minute per address, shared with the hooks in the session
/// this was typed into. Stopping keeps the rest of that budget for them, and
/// what was already sent stays sent — which is what makes "run it again"
/// true rather than a hope.
#[tokio::test]
async fn a_refusal_stops_the_run_and_says_what_to_do_about_it() {
    let fx = Fixture::new().await;
    write(&fx.memory("a.md"), "one\n");
    write(&fx.memory("b.md"), "two\n");
    fx.server
        .fail_pushes_with(429, r#"{"error":"rate limit exceeded, try again later"}"#);

    let out = backfill(&fx.ctx).await.unwrap();

    let stopped = out
        .stopped
        .clone()
        .expect("the run should report why it ended");
    assert!(
        stopped.contains("rate limit") && stopped.contains("again"),
        "it has to name the cause and the remedy: {stopped}"
    );
    assert_eq!(
        fx.server.push_attempts(),
        1,
        "it must stop at the first refusal rather than spending the rest of a \
         budget the session's own hooks are sharing"
    );
    assert_eq!(out.not_reached, 1, "and say how many it did not get to");
    assert_eq!(out.count(Disposition::Sent), 0);
    assert!(
        out.baseline.is_some(),
        "a half-finished comparison must not leave a baseline behind, and has \
         to say it did not"
    );
    assert!(
        crate::state::load(&fx.ctx.state_file).unwrap().is_none(),
        "writing one here would let a later local delete tombstone a file this \
         run never even looked at"
    );
}

/// A name this client refuses before sending — and therefore one the server
/// would refuse too — must not take the rest of the directory down with it.
/// `list_memory_files` sorts, so a single bad name would otherwise make every
/// file after it permanently unsendable, run after run.
///
/// Unix only, because the file cannot exist on Windows: every name
/// `validate_file_path` refuses needs a `:` at byte 1 or a backslash, and
/// Windows reads the first as a data stream and the second as a separator.
/// A Windows memory directory can therefore never hold a file the server
/// refuses on its name, which is the situation this guards.
#[cfg(unix)]
#[tokio::test]
async fn one_file_the_server_will_never_accept_does_not_wedge_the_rest() {
    let fx = Fixture::new().await;
    write(&fx.memory("a.md"), "first\n");
    // Rejected by `validate_file_path`: byte 1 is a colon, which reads as a
    // Windows drive prefix. A perfectly legal filename on this machine.
    write(&fx.memory("a:b.md"), "the awkward one\n");
    write(&fx.memory("zzz.md"), "last\n");

    let out = backfill(&fx.ctx).await.unwrap();

    assert!(out.stopped.is_none(), "{:?}", out.stopped);
    assert_eq!(disposition_of(&out, "a:b.md"), Disposition::Refused);
    assert_eq!(
        disposition_of(&out, "zzz.md"),
        Disposition::Sent,
        "the file sorted after the refused one still has to go"
    );
    assert!(
        out.entries
            .iter()
            .find(|e| e.path == "a:b.md")
            .unwrap()
            .detail
            .is_some(),
        "and the refusal has to say why, or there is nothing to act on"
    );
}

/// Without a baseline, `push` cannot tell a deleted file from one that was
/// never there — and a machine whose memory predates Recall is exactly the
/// machine that has no baseline. Writing one is half of what a first sync is
/// for.
#[tokio::test]
async fn a_backfill_leaves_the_baseline_a_later_push_reconciles_against() {
    let fx = Fixture::new().await;
    write(&fx.memory("topics/auth.md"), "bearer tokens\n");

    assert!(
        crate::state::load(&fx.ctx.state_file).unwrap().is_none(),
        "the fixture starts with no baseline, like a fresh install"
    );

    backfill(&fx.ctx).await.unwrap();

    let baseline = crate::state::load(&fx.ctx.state_file)
        .unwrap()
        .expect("a backfill should leave one behind");
    assert_eq!(baseline.files, ["topics/auth.md"]);
}

/// Asking the server what it holds is not optional. If that call fails there
/// is no way to know what would be overwritten, so the run must not fall
/// back to sending blind.
#[tokio::test]
async fn a_backfill_that_cannot_read_the_server_sends_nothing() {
    let fx = Fixture::new().await;
    write(&fx.memory("topics/auth.md"), "bearer tokens\n");
    fx.server.fail_with(500, "boom");

    let err = backfill(&fx.ctx).await.unwrap_err();

    assert!(
        err.to_string().contains("acme/app"),
        "the error should name the scope it could not read: {err}"
    );
    assert!(fx.server.pushes().is_empty());
}
