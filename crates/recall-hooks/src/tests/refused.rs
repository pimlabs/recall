//! A change the server did not take.
//!
//! What is on disk and differs from the baseline is the queue: nothing else
//! is kept, and every push, pull and `recall sync` sends what it finds there
//! first. These pin both halves of that: a run waits out the server's rate
//! limit rather than leaving the change behind, and whatever it could not
//! send stays visible to the next run, whichever run that is.

use std::fs;

use recall_wire::{content_sha256, File};

use super::{write, Fixture};
use crate::context::Error;
use crate::pull::pull;
use crate::push::{push, unsent};
use crate::state;

fn file(path: &str, content: &str) -> File {
    File {
        file_path: path.into(),
        content: Some(content.into()),
        ..Default::default()
    }
}

const RATE_LIMITED: &str = r#"{"error":"rate limit exceeded, try again later"}"#;

/// Long enough for a refused run to have gone round again: a change made
/// during a wait is seen by the next attempt, not by one already under way.
async fn settle() {
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
}

/// A burst past the limit used to lose the edit that met it: the hook failed
/// once and nothing ran again until the next memory edit. Mutation: send
/// once, with no wait.
#[tokio::test]
async fn a_push_the_rate_limit_refuses_goes_out_when_the_window_opens() {
    let f = Fixture::new().await;
    write(&f.memory("notes.md"), "one\n");
    f.server.refuse_next_pushes(2);

    let res = push(&f.ctx, &f.memory("notes.md")).await.unwrap();

    assert_eq!(res.pushed.as_deref(), Some("notes.md"));
    assert_eq!(f.server.push_attempts(), 3);
    assert_eq!(f.server.pushes().len(), 1);
}

/// So does the sweep, which is what meets the limit first: `MEMORY.md`
/// sorts ahead of everything, and it is what the real refusals named.
#[tokio::test]
async fn a_sweep_the_rate_limit_refuses_carries_on_when_the_window_opens() {
    let f = Fixture::new().await;
    f.server
        .set_files(vec![file("a.md", "v1\n"), file("b.md", "v1\n")]);
    pull(&f.ctx).await.unwrap();

    fs::write(f.memory("a.md"), "v2\n").unwrap();
    fs::write(f.memory("b.md"), "v2\n").unwrap();
    f.server.refuse_next_pushes(1);

    let res = push(&f.ctx, &f.memory("b.md")).await.unwrap();

    assert_eq!(res.swept, vec!["a.md"]);
    assert_eq!(res.pushed.as_deref(), Some("b.md"));
    assert_eq!(f.server.pushes().len(), 2);
}

/// Waiting is for the one answer that means "later". A file the server will
/// not take is not going to change its mind, and a hook sitting out a minute
/// for it would hold the session up for nothing.
#[tokio::test]
async fn only_the_rate_limit_is_waited_out() {
    let f = Fixture::new().await;
    write(&f.memory("notes.md"), "one\n");
    f.server
        .fail_pushes_with(400, r#"{"error":"file_path is invalid"}"#);

    let err = push(&f.ctx, &f.memory("notes.md")).await.unwrap_err();

    assert!(matches!(err, Error::Push { .. }), "{err:?}");
    assert_eq!(f.server.push_attempts(), 1);
}

/// A limit outlasting the wait fails the run, loudly, and the change stays
/// where the next run finds it. Mutation: refresh the baseline on the way
/// out as though the push had worked.
#[tokio::test]
async fn a_limit_that_outlasts_the_wait_leaves_the_change_for_the_next_run() {
    let f = Fixture::new().await;
    f.server.set_files(vec![file("a.md", "v1\n")]);
    pull(&f.ctx).await.unwrap();

    fs::write(f.memory("a.md"), "v2\n").unwrap();
    f.server.fail_pushes_with(429, RATE_LIMITED);
    push(&f.ctx, &f.memory("a.md")).await.unwrap_err();
    assert!(f.server.pushes().is_empty());

    f.server.stop_failing_pushes();
    write(&f.memory("b.md"), "new\n");
    let res = push(&f.ctx, &f.memory("b.md")).await.unwrap();

    assert_eq!(res.swept, vec!["a.md"]);
    let a = f.server.pushes().remove(0);
    assert_eq!(a.content.as_deref(), Some("v2\n"));
    assert_eq!(
        a.base_sha256.as_deref(),
        Some(content_sha256("v1\n").as_str())
    );
}

/// `recall sync`, and the pull at the next session start, are the other
/// runs that can clear it, which is what makes the error's advice true.
#[tokio::test]
async fn a_pull_sends_what_a_refused_push_left() {
    let f = Fixture::new().await;
    f.server.set_files(vec![file("a.md", "v1\n")]);
    pull(&f.ctx).await.unwrap();

    write(&f.memory("new.md"), "new\n");
    f.server.fail_pushes_with(429, RATE_LIMITED);
    push(&f.ctx, &f.memory("new.md")).await.unwrap_err();

    f.server.stop_failing_pushes();
    let res = pull(&f.ctx).await.unwrap();

    assert_eq!(res.sent, vec!["new.md"]);
}

/// A sweep cut short has still sent what it sent. Forgetting that leaves
/// each of those files with its old base, so the next edit to one looks
/// concurrent with itself, and the server merges it: a line deleted on
/// purpose comes back. Mutation: return from a failed run writing nothing.
#[tokio::test]
async fn a_sweep_cut_short_keeps_the_base_of_what_it_sent() {
    let f = Fixture::new().await;
    f.server
        .set_files(vec![file("a.md", "v1\n"), file("b.md", "v1\n")]);
    pull(&f.ctx).await.unwrap();

    fs::write(f.memory("a.md"), "v2\n").unwrap();
    fs::write(f.memory("b.md"), "v2\n").unwrap();
    write(&f.memory("c.md"), "new\n");
    f.server.limit_pushes_after(1);
    push(&f.ctx, &f.memory("c.md")).await.unwrap_err();

    let base = state::load(&f.ctx.state_file).unwrap().unwrap();
    assert_eq!(
        base.bases.get("a.md"),
        Some(&content_sha256("v2\n")),
        "a.md went out, and its base has to say so"
    );
    assert_eq!(
        base.bases.get("b.md"),
        Some(&content_sha256("v1\n")),
        "b.md did not"
    );

    f.server.stop_failing_pushes();
    fs::write(f.memory("a.md"), "v3\n").unwrap();
    push(&f.ctx, &f.memory("a.md")).await.unwrap();

    let sent: Vec<_> = f
        .server
        .pushes()
        .into_iter()
        .filter(|p| p.file_path == "a.md" && p.content.as_deref() == Some("v3\n"))
        .collect();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].base_sha256.as_deref(),
        Some(content_sha256("v2\n").as_str()),
    );
}

/// A delete is a change like any other: refused, it stays in the baseline
/// to be sent again, and sent, it leaves. Mutation: drop the deleted paths
/// from the baseline before they are sent.
#[tokio::test]
async fn a_refused_delete_is_sent_by_the_next_run() {
    let f = Fixture::new().await;
    f.server
        .set_files(vec![file("a.md", "v1\n"), file("gone.md", "v1\n")]);
    pull(&f.ctx).await.unwrap();

    fs::remove_file(f.memory("gone.md")).unwrap();
    fs::write(f.memory("a.md"), "v2\n").unwrap();
    f.server.fail_pushes_with(429, RATE_LIMITED);
    let err = push(&f.ctx, &f.memory("a.md")).await.unwrap_err();
    assert!(matches!(err, Error::PushDelete { .. }), "{err:?}");

    f.server.stop_failing_pushes();
    let res = push(&f.ctx, &f.memory("a.md")).await.unwrap();

    assert_eq!(res.deleted, vec!["gone.md"]);
}

/// With no baseline a run sweeps nothing, so a new file refused on the very
/// first run had nobody left to send it: the next hook wrote a baseline
/// listing it, and a listed file with no base reads as unchanged for ever.
/// Not even `recall sync` could send it. Mutation: leave the baseline
/// unwritten when a first run is refused.
#[tokio::test]
async fn a_new_file_refused_on_the_first_run_is_not_forgotten() {
    let f = Fixture::new().await;
    write(&f.memory("first.md"), "one\n");
    f.server.fail_pushes_with(429, RATE_LIMITED);
    push(&f.ctx, &f.memory("first.md")).await.unwrap_err();

    f.server.stop_failing_pushes();
    write(&f.memory("second.md"), "two\n");
    let res = push(&f.ctx, &f.memory("second.md")).await.unwrap();

    assert_eq!(res.swept, vec!["first.md"]);
}

/// A first run refused does not make everything on disk new: what was
/// there before it is left to `recall backfill`, as a first run leaves it.
#[tokio::test]
async fn a_first_run_refused_sweeps_only_the_file_it_was_for() {
    let f = Fixture::new().await;
    write(&f.memory("old.md"), "already here\n");
    write(&f.memory("first.md"), "one\n");
    f.server.fail_pushes_with(429, RATE_LIMITED);
    push(&f.ctx, &f.memory("first.md")).await.unwrap_err();

    f.server.stop_failing_pushes();
    write(&f.memory("second.md"), "two\n");
    let res = push(&f.ctx, &f.memory("second.md")).await.unwrap();

    assert_eq!(res.swept, vec!["first.md"]);
}

/// Two hooks at once: this one's file is written and its push is yet to
/// finish when the other rewrites the baseline. Listing the file then would
/// tell every later run it has nothing to send. Mutation: list everything
/// on disk in `refresh_state_with`.
#[tokio::test]
async fn a_new_file_another_run_has_not_sent_stays_off_the_baseline() {
    let f = Fixture::new().await;
    f.server.set_files(vec![file("a.md", "v1\n")]);
    pull(&f.ctx).await.unwrap();

    write(&f.memory("late.md"), "not sent yet\n");
    f.ctx.refresh_state_with(&[]).unwrap();

    let res = pull(&f.ctx).await.unwrap();
    assert_eq!(res.sent, vec!["late.md"]);
}

/// A file listed with no base is what a first run leaves for everything
/// already on disk, and reads as unchanged. Editing one and having that push
/// refused has to change that, or nothing ever sends the edit. Mutation:
/// leave `refused` listed when a baseline exists.
#[tokio::test]
async fn an_edit_refused_on_a_file_listed_with_no_base_is_sent_by_the_next_run() {
    let f = Fixture::new().await;
    write(&f.memory("old.md"), "there before the first run\n");
    write(&f.memory("first.md"), "one\n");
    push(&f.ctx, &f.memory("first.md")).await.unwrap();

    fs::write(f.memory("old.md"), "edited\n").unwrap();
    f.server.fail_pushes_with(500, r#"{"error":"internal"}"#);
    push(&f.ctx, &f.memory("old.md")).await.unwrap_err();
    let names = || unsent(&f.ctx.memory_dir, &f.ctx.state_file, &f.ctx.scopes).unwrap();
    assert_eq!(names(), vec!["old.md"]);

    f.server.stop_failing_pushes();
    write(&f.memory("next.md"), "two\n");
    let res = push(&f.ctx, &f.memory("next.md")).await.unwrap();

    assert_eq!(res.swept, vec!["old.md"]);
}

/// A delete that went out before the run failed takes the file's base and
/// fingerprint with it. Left behind, the same bytes restored by `git
/// checkout` read as unchanged, and the next pull applies the tombstone to
/// them. Mutation: keep `bases` and `disk` for a deleted path.
#[tokio::test]
async fn a_file_restored_after_its_delete_went_out_is_sent_again() {
    let f = Fixture::new().await;
    f.server
        .set_files(vec![file("gone.md", "v1\n"), file("z.md", "v1\n")]);
    pull(&f.ctx).await.unwrap();

    fs::remove_file(f.memory("gone.md")).unwrap();
    fs::write(f.memory("z.md"), "v2\n").unwrap();
    f.server.limit_pushes_after(1);
    write(&f.memory("new.md"), "new\n");
    push(&f.ctx, &f.memory("new.md")).await.unwrap_err();
    f.server.stop_failing_pushes();

    fs::write(f.memory("gone.md"), "v1\n").unwrap();
    let res = push(&f.ctx, &f.memory("new.md")).await.unwrap();

    assert!(res.swept.contains(&"gone.md".to_string()), "{res:?}");
}

/// The wait is a minute long, and another process can change a file in it.
/// Each attempt reads the directory afresh, so what goes out is what is
/// there then, and the baseline ends up agreeing with it. Mutation: read
/// the changes once, before the first attempt.
#[tokio::test]
async fn a_change_made_during_the_wait_goes_out_as_it_now_stands() {
    let f = Fixture::new().await;
    f.server
        .set_files(vec![file("a.md", "v1\n"), file("b.md", "v1\n")]);
    pull(&f.ctx).await.unwrap();

    fs::write(f.memory("a.md"), "v2\n").unwrap();
    write(&f.memory("t.md"), "trigger\n");
    f.server.fail_pushes_with(429, RATE_LIMITED);

    let during_the_wait = async {
        settle().await;
        fs::write(f.memory("a.md"), "v3 from the shell\n").unwrap();
        fs::write(f.memory("b.md"), "v2 from the shell\n").unwrap();
        settle().await;
        f.server.stop_failing_pushes();
    };
    let trigger = f.memory("t.md");
    let (res, ()) = tokio::join!(push(&f.ctx, &trigger), during_the_wait);
    res.unwrap();

    let contents: Vec<_> = f
        .server
        .pushes()
        .into_iter()
        .filter_map(|p| Some((p.file_path, p.content?)))
        .collect();
    assert!(
        contents.contains(&("a.md".into(), "v3 from the shell\n".into())),
        "{contents:?}"
    );
    assert!(
        contents.contains(&("b.md".into(), "v2 from the shell\n".into())),
        "{contents:?}"
    );
    assert!(
        !contents.contains(&("a.md".into(), "v2\n".into())),
        "{contents:?}"
    );
    assert!(unsent(&f.ctx.memory_dir, &f.ctx.state_file, &f.ctx.scopes)
        .unwrap()
        .is_empty());
}

/// A file back on disk by the time the wait ends is not deleted: a tombstone
/// sent then would take it away at the next pull. Mutation: decide the
/// deletes once, before the first attempt.
#[tokio::test]
async fn a_file_that_comes_back_during_the_wait_is_not_deleted() {
    let f = Fixture::new().await;
    f.server.set_files(vec![file("back.md", "v1\n")]);
    pull(&f.ctx).await.unwrap();

    fs::remove_file(f.memory("back.md")).unwrap();
    write(&f.memory("t.md"), "trigger\n");
    f.server.fail_pushes_with(429, RATE_LIMITED);

    let during_the_wait = async {
        settle().await;
        fs::write(f.memory("back.md"), "v1\n").unwrap();
        settle().await;
        f.server.stop_failing_pushes();
    };
    let trigger = f.memory("t.md");
    let (res, ()) = tokio::join!(push(&f.ctx, &trigger), during_the_wait);
    res.unwrap();

    assert!(
        !f.server.pushes().iter().any(|p| p.deleted),
        "{:?}",
        f.server.pushes()
    );
}

/// And a file removed during the wait gets its delete. Mutation: rebuild
/// the baseline from disk without a fresh look at what vanished.
#[tokio::test]
async fn a_file_removed_during_the_wait_is_deleted() {
    let f = Fixture::new().await;
    f.server.set_files(vec![file("c.md", "v1\n")]);
    pull(&f.ctx).await.unwrap();

    write(&f.memory("t.md"), "trigger\n");
    f.server.fail_pushes_with(429, RATE_LIMITED);

    let during_the_wait = async {
        settle().await;
        fs::remove_file(f.memory("c.md")).unwrap();
        settle().await;
        f.server.stop_failing_pushes();
    };
    let trigger = f.memory("t.md");
    let (res, ()) = tokio::join!(push(&f.ctx, &trigger), during_the_wait);

    assert_eq!(res.unwrap().deleted, vec!["c.md"]);
}

/// What was sent stays recorded whatever ended the run, not only a refusal:
/// here a triggering file that cannot be read as text, after a sweep that
/// went out. Mutation: record only on a server error.
#[tokio::test]
async fn a_run_ended_by_a_local_error_keeps_what_it_sent() {
    let f = Fixture::new().await;
    f.server.set_files(vec![file("a.md", "v1\n")]);
    pull(&f.ctx).await.unwrap();

    fs::write(f.memory("a.md"), "v2\n").unwrap();
    fs::write(f.memory("bad.md"), [0xff, 0xfe]).unwrap();
    let err = push(&f.ctx, &f.memory("bad.md")).await.unwrap_err();
    assert!(matches!(err, Error::NotUtf8 { .. }), "{err:?}");

    let base = state::load(&f.ctx.state_file).unwrap().unwrap();
    assert_eq!(base.bases.get("a.md"), Some(&content_sha256("v2\n")));
}

/// A pull's sweep can go out and its fetch fail, and a sweep is what spends
/// the window the fetch then meets. Mutation: keep what was sent only when
/// the sweep itself fails.
#[tokio::test]
async fn a_pull_whose_fetch_fails_keeps_what_its_sweep_sent() {
    let f = Fixture::new().await;
    f.server.set_files(vec![file("a.md", "v1\n")]);
    pull(&f.ctx).await.unwrap();

    fs::write(f.memory("a.md"), "v2\n").unwrap();
    f.server
        .refuse_next_pulls(1, 500, r#"{"error":"internal"}"#);
    let err = pull(&f.ctx).await.unwrap_err();
    assert!(matches!(err, Error::Pull { .. }), "{err:?}");

    let base = state::load(&f.ctx.state_file).unwrap().unwrap();
    assert_eq!(base.bases.get("a.md"), Some(&content_sha256("v2\n")));
}

/// A session that starts inside a spent window gets its memory late rather
/// than not at all. Mutation: wait out a refused sweep but not a fetch.
#[tokio::test]
async fn a_pull_the_rate_limit_refuses_is_asked_again() {
    let f = Fixture::new().await;
    f.server.set_files(vec![file("a.md", "v1\n")]);
    f.server.refuse_next_pulls(2, 429, RATE_LIMITED);

    let res = pull(&f.ctx).await.unwrap();

    assert_eq!(res.written, vec!["a.md"]);
}

/// What a retried pull sent stays sent: the local file is the newer of the
/// two, and the reported count says so.
#[tokio::test]
async fn a_pull_asked_again_remembers_what_it_sent_first() {
    let f = Fixture::new().await;
    f.server.set_files(vec![file("a.md", "v1\n")]);
    pull(&f.ctx).await.unwrap();

    fs::write(f.memory("a.md"), "v2\n").unwrap();
    f.server.refuse_next_pulls(1, 429, RATE_LIMITED);
    let res = pull(&f.ctx).await.unwrap();

    assert_eq!(res.sent, vec!["a.md"]);
    assert_eq!(fs::read_to_string(f.memory("a.md")).unwrap(), "v2\n");
}

/// A retry goes over what an earlier attempt wrote, and the count is of
/// files, not of writes. Mutation: push to `written` unconditionally.
#[tokio::test]
async fn a_pull_asked_again_counts_each_file_once() {
    let f = Fixture::with_machine_scope().await;
    f.server.set_files(vec![file("a.md", "v1\n")]);
    f.server.refuse_next_pulls(0, 429, RATE_LIMITED);
    pull(&f.ctx).await.unwrap();
    let first = f.server.pulled_keys().len();

    fs::remove_file(f.memory("a.md")).unwrap();
    fs::remove_file(&f.ctx.state_file).unwrap();
    f.server.set_files(vec![file("a.md", "v2\n")]);
    // The second of the two scopes is what is refused.
    f.server.refuse_next_pulls_after(1, 1, 429, RATE_LIMITED);
    let res = pull(&f.ctx).await.unwrap();

    assert!(f.server.pulled_keys().len() > first + 2);
    assert_eq!(res.written.iter().filter(|r| *r == "a.md").count(), 1);
}

/// What a pull wrote before a later scope was refused is the server's, not
/// a local edit. Unrecorded, the retry's sweep sends it back from its old
/// base, and a line another machine removed on purpose can return in the
/// merge. Mutation: record only what was sent when a pull fails.
#[tokio::test]
async fn a_pull_asked_again_does_not_send_back_what_it_wrote() {
    let f = Fixture::with_machine_scope().await;
    f.server.set_files(vec![file("x.md", "v1\n")]);
    pull(&f.ctx).await.unwrap();

    f.server.set_files(vec![
        file("x.md", "v2 from elsewhere\n"),
        file("y.md", "new\n"),
    ]);
    f.server.refuse_next_pulls_after(1, 1, 429, RATE_LIMITED);
    let res = pull(&f.ctx).await.unwrap();

    assert!(res.sent.is_empty(), "{res:?}");
    assert!(f.server.pushes().is_empty(), "{:?}", f.server.pushes());
}

/// The same when no retry follows: the next run must not take them for
/// edits either.
#[tokio::test]
async fn a_pull_stopped_after_one_scope_records_what_it_wrote() {
    let f = Fixture::with_machine_scope().await;
    f.server.set_files(vec![file("x.md", "v1\n")]);
    pull(&f.ctx).await.unwrap();

    f.server
        .set_files(vec![file("x.md", "v2 from elsewhere\n")]);
    f.server
        .refuse_next_pulls_after(1, 1, 500, r#"{"error":"internal"}"#);
    pull(&f.ctx).await.unwrap_err();

    let names = unsent(&f.ctx.memory_dir, &f.ctx.state_file, &f.ctx.scopes).unwrap();
    assert!(names.is_empty(), "{names:?}");
}

/// A tombstone applied before a later scope was refused is the server's
/// too: the file is gone because the server said so, and sending that back
/// would be a delete nobody made. Mutation: leave `res.removed` listed.
#[tokio::test]
async fn a_pull_asked_again_does_not_send_a_delete_it_was_told_of() {
    let f = Fixture::with_machine_scope().await;
    f.server.set_files(vec![file("x.md", "v1\n")]);
    pull(&f.ctx).await.unwrap();

    f.server.set_files(vec![File {
        file_path: "x.md".into(),
        deleted: true,
        ..Default::default()
    }]);
    f.server.refuse_next_pulls_after(1, 1, 429, RATE_LIMITED);
    let res = pull(&f.ctx).await.unwrap();

    assert!(res.sent_deletes.is_empty(), "{res:?}");
    assert!(f.server.pushes().is_empty(), "{:?}", f.server.pushes());
}

/// Taking a refused file off the list would lose track of it another way:
/// listed is how a later delete is noticed. Mutation: unlist it.
#[tokio::test]
async fn a_delete_after_a_refused_edit_of_a_file_with_no_base_is_sent() {
    let f = Fixture::new().await;
    write(&f.memory("old.md"), "there before the first run\n");
    write(&f.memory("first.md"), "one\n");
    push(&f.ctx, &f.memory("first.md")).await.unwrap();

    fs::write(f.memory("old.md"), "edited\n").unwrap();
    f.server.fail_pushes_with(500, r#"{"error":"internal"}"#);
    push(&f.ctx, &f.memory("old.md")).await.unwrap_err();
    fs::remove_file(f.memory("old.md")).unwrap();

    f.server.stop_failing_pushes();
    write(&f.memory("next.md"), "two\n");
    let res = push(&f.ctx, &f.memory("next.md")).await.unwrap();

    assert_eq!(res.deleted, vec!["old.md"]);
}

/// A `MEMORY.md` holding nothing but Recall's own links is derived: every
/// machine regenerates it, and those links lead nowhere from the project's
/// scope. It stays off the baseline like any file nobody sent, and must not
/// read as new. Mutation: drop the exception in `changed_since_sync`.
#[tokio::test]
async fn a_memory_md_of_nothing_but_index_lines_is_not_sent() {
    let f = Fixture::with_global_scope().await;
    write(&f.memory("a.md"), "one\n");
    push(&f.ctx, &f.memory("a.md")).await.unwrap();

    f.server
        .set_files_for("global:eko", vec![file("editor.md", "tabs\n")]);
    pull(&f.ctx).await.unwrap();
    assert!(f.memory("MEMORY.md").exists());

    write(&f.memory("b.md"), "two\n");
    let res = push(&f.ctx, &f.memory("b.md")).await.unwrap();

    assert!(!res.swept.contains(&"MEMORY.md".to_string()), "{res:?}");
    let names = unsent(&f.ctx.memory_dir, &f.ctx.state_file, &f.ctx.scopes).unwrap();
    assert!(names.is_empty(), "{names:?}");
}

/// And one somebody wrote in is theirs, and goes out as any new file does.
#[tokio::test]
async fn a_memory_md_with_something_of_its_own_is_sent() {
    let f = Fixture::with_global_scope().await;
    write(&f.memory("a.md"), "one\n");
    push(&f.ctx, &f.memory("a.md")).await.unwrap();

    write(&f.memory("MEMORY.md"), "- a note of my own\n");
    write(&f.memory("b.md"), "two\n");
    let res = push(&f.ctx, &f.memory("b.md")).await.unwrap();

    assert!(res.swept.contains(&"MEMORY.md".to_string()), "{res:?}");
}

/// `recall backfill` looks at every file and decides. Holding a file back
/// because the server's copy differs is a decision, and a marker left from a
/// refused push must not undo it at the next sweep. Mutation: keep
/// `NEVER_SENT` through `refresh_state`.
#[tokio::test]
async fn a_backfill_decides_for_a_file_a_refused_push_marked() {
    let f = Fixture::new().await;
    write(&f.memory("old.md"), "there before the first run\n");
    write(&f.memory("first.md"), "one\n");
    push(&f.ctx, &f.memory("first.md")).await.unwrap();

    fs::write(f.memory("old.md"), "edited\n").unwrap();
    f.server.fail_pushes_with(500, r#"{"error":"internal"}"#);
    push(&f.ctx, &f.memory("old.md")).await.unwrap_err();

    f.server.stop_failing_pushes();
    f.server
        .set_files(vec![file("old.md", "the server's own\n")]);
    crate::backfill::backfill(&f.ctx).await.unwrap();

    write(&f.memory("next.md"), "two\n");
    let res = push(&f.ctx, &f.memory("next.md")).await.unwrap();

    assert!(!res.swept.contains(&"old.md".to_string()), "{res:?}");
}

/// Two scopes can name one local path, through a row from before the global
/// scope. Written by one and removed by the next, it is gone, and recording
/// it as written lists a file that is not there: the next sweep then sends a
/// delete for the real note. Mutation: leave it among what was written.
#[tokio::test]
async fn a_file_written_then_removed_by_one_pull_is_not_recorded_as_written() {
    let f = Fixture::with_global_scope().await;
    write(&f.memory("a"), "a plain file where a directory is wanted\n");
    write(&f.memory("seed.md"), "s\n");
    push(&f.ctx, &f.memory("seed.md")).await.unwrap();

    f.server
        .set_files_for("global:eko", vec![file("x.md", "global note\n")]);
    f.server.set_files(vec![
        File {
            file_path: "global/x.md".into(),
            deleted: true,
            ..Default::default()
        },
        file("a/b.md", "cannot be written\n"),
    ]);
    pull(&f.ctx).await.unwrap_err();
    assert!(!f.memory("global/x.md").exists());

    write(&f.memory("c.md"), "c\n");
    let res = push(&f.ctx, &f.memory("c.md")).await.unwrap();

    assert!(res.deleted.is_empty(), "{res:?}");
}

/// What `recall doctor` reports: the same changes a run would send.
#[tokio::test]
async fn unsent_names_what_the_next_run_would_send() {
    let f = Fixture::new().await;
    f.server.set_files(vec![
        file("a.md", "v1\n"),
        file("b.md", "v1\n"),
        file("gone.md", "v1\n"),
    ]);
    pull(&f.ctx).await.unwrap();
    let names = || unsent(&f.ctx.memory_dir, &f.ctx.state_file, &f.ctx.scopes).unwrap();
    assert!(names().is_empty(), "{:?}", names());

    fs::write(f.memory("a.md"), "v2\n").unwrap();
    write(&f.memory("new.md"), "new\n");
    fs::remove_file(f.memory("gone.md")).unwrap();
    assert_eq!(names(), vec!["a.md", "gone.md", "new.md"]);
}

/// A machine that has never synced has nothing to call unsent: with no
/// baseline there is no "since".
#[tokio::test]
async fn nothing_is_unsent_before_the_first_sync() {
    let f = Fixture::new().await;
    write(&f.memory("a.md"), "one\n");
    assert!(unsent(&f.ctx.memory_dir, &f.ctx.state_file, &f.ctx.scopes)
        .unwrap()
        .is_empty());
}
