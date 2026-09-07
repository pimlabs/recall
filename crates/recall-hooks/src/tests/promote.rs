//! Promoting a project note into the global scope.
//!
//! Two things are worth protecting. The first is routing, as everywhere else
//! scopes are involved: the note has to be stored under the *global* key and
//! its old path tombstoned under the *project* key, and getting either
//! backwards is a one-way door once it reaches the server. The second is
//! that a promotion is a move made of four steps, three of which can fail —
//! and no failure may leave the note at neither path.

use super::{write, Fixture};
use crate::context::Error as SyncError;
use crate::promote::{promote, Error};
use crate::state;

/// Front matter Claude Code would actually have written on a note about the
/// user, so the index has a description to work with.
const NOTE: &str = "---\ntype: user\ndescription: \"Prefers helix\"\n---\n\nprefers helix\n";

fn read(path: &std::path::Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

#[tokio::test]
async fn a_promoted_note_leaves_the_project_and_lands_in_the_global_subtree() {
    let f = Fixture::with_global_scope().await;
    write(&f.memory("user.md"), NOTE);

    let res = promote(&f.ctx, &f.memory("user.md")).await.unwrap();

    assert_eq!(res.from, "user.md");
    assert_eq!(res.to, "global/user.md");
    assert!(!res.resumed);
    assert!(
        !f.memory("user.md").exists(),
        "the project copy survived, so the note is now in two scopes"
    );
    assert_eq!(read(&f.memory("global/user.md")), NOTE);
}

#[tokio::test]
async fn the_note_is_stored_under_the_global_key_and_its_old_path_tombstoned_under_the_project_key()
{
    let f = Fixture::with_global_scope().await;
    write(&f.memory("user.md"), NOTE);

    promote(&f.ctx, &f.memory("user.md")).await.unwrap();

    // Both requests name `user.md`; only the key and the flag tell them
    // apart, which is exactly the confusion worth testing.
    let pushes = f.server.pushes();
    let stored = pushes
        .iter()
        .find(|p| !p.deleted)
        .unwrap_or_else(|| panic!("the note was never stored: {pushes:?}"));
    assert_eq!(stored.project_key, "global:eko");
    assert_eq!(stored.file_path, "user.md");
    assert_eq!(stored.content.as_deref(), Some(NOTE));

    let tombstone = pushes
        .iter()
        .find(|p| p.deleted)
        .unwrap_or_else(|| panic!("the old path was never tombstoned: {pushes:?}"));
    assert_eq!(
        tombstone.project_key, "acme/app",
        "the tombstone went to the wrong scope, so this project keeps serving the old copy"
    );
    assert_eq!(tombstone.file_path, "user.md");
    assert!(tombstone.content.is_none());
}

/// Without the link the file is on disk and Claude Code never reads it,
/// which is the entire point of promoting it.
#[tokio::test]
async fn a_promoted_note_is_linked_from_memory_md() {
    let f = Fixture::with_global_scope().await;
    write(
        &f.memory("MEMORY.md"),
        "# Memory\n\n- [Auth](topics/auth.md) — how auth works\n",
    );
    write(&f.memory("user.md"), NOTE);

    promote(&f.ctx, &f.memory("user.md")).await.unwrap();

    let memory_md = read(&f.memory("MEMORY.md"));
    assert!(
        memory_md.contains("- [user](global/user.md) — Prefers helix"),
        "MEMORY.md must link the promoted note, or it is never read:\n{memory_md}"
    );
    assert!(
        memory_md.contains("- [Auth](topics/auth.md) — how auth works"),
        "the project's own index entries were lost:\n{memory_md}"
    );
}

/// The line the project wrote about the note points at a path the promotion
/// just emptied. A dead link is worse than no link: the model spends a read
/// on it and gets nothing back.
#[tokio::test]
async fn the_projects_own_link_to_the_note_goes_with_it() {
    let f = Fixture::with_global_scope().await;
    write(
        &f.memory("MEMORY.md"),
        "# Memory\n\n- [Commit style](topics/user.md) — how they word commits\n         - [Auth](topics/auth.md) — how auth works\n",
    );
    write(&f.memory("topics/user.md"), NOTE);

    promote(&f.ctx, &f.memory("topics/user.md")).await.unwrap();

    let memory_md = read(&f.memory("MEMORY.md"));
    assert!(
        !memory_md.contains("topics/user.md"),
        "the index still links the path the note left:\n{memory_md}"
    );
    assert!(
        memory_md.contains("- [user](global/user.md) — Prefers helix"),
        "the note has to be linked at its new path:\n{memory_md}"
    );
    assert!(
        memory_md.contains("- [Auth](topics/auth.md) — how auth works"),
        "only the one dead link may go:\n{memory_md}"
    );
}

/// `MEMORY.md` is itself a synced project file, so a line removed only on
/// disk comes straight back with the next pull — here, and on every other
/// machine, which never even saw the removal.
#[tokio::test]
async fn the_rewritten_index_is_pushed_so_the_dead_link_cannot_come_back() {
    let f = Fixture::with_global_scope().await;
    write(
        &f.memory("MEMORY.md"),
        "# Memory\n\n- [Commit style](topics/user.md)\n",
    );
    write(&f.memory("topics/user.md"), NOTE);

    promote(&f.ctx, &f.memory("topics/user.md")).await.unwrap();

    let pushes = f.server.pushes();
    let index = pushes
        .iter()
        .find(|p| p.file_path == "MEMORY.md")
        .unwrap_or_else(|| panic!("the rewritten index was never pushed: {pushes:?}"));
    assert_eq!(index.project_key, "acme/app");
    assert!(!index.deleted);
    let body = index.content.as_deref().unwrap_or_default();
    assert!(
        !body.contains("topics/user.md"),
        "the pushed index still carries the dead link:\n{body}"
    );
}

/// The index only changes when it linked the note, so the ordinary promotion
/// stays three requests rather than four — and does not push `MEMORY.md`
/// bearing this machine's global links to a project that did not ask for it.
#[tokio::test]
async fn an_index_that_did_not_mention_the_note_is_not_pushed() {
    let f = Fixture::with_global_scope().await;
    write(&f.memory("user.md"), NOTE);

    promote(&f.ctx, &f.memory("user.md")).await.unwrap();

    let pushes = f.server.pushes();
    assert!(
        !pushes.iter().any(|p| p.file_path == "MEMORY.md"),
        "nothing in the index was invalidated, so nothing was owed: {pushes:?}"
    );
}

/// The baseline has to describe the new layout before the run ends, or the
/// next push hook reads the move as a delete and tombstones the note under
/// the project key a second time.
#[tokio::test]
async fn the_baseline_ends_the_run_describing_the_new_layout() {
    let f = Fixture::with_global_scope().await;
    write(&f.memory("user.md"), NOTE);

    promote(&f.ctx, &f.memory("user.md")).await.unwrap();

    let baseline = state::load(&f.ctx.state_file).unwrap().unwrap();
    assert!(
        baseline.files.contains(&"global/user.md".to_string()),
        "{baseline:?}"
    );
    assert!(
        !baseline.files.contains(&"user.md".to_string()),
        "the old path is still in the baseline: {baseline:?}"
    );
}

/// A project's folders describe that repository, so they are dropped rather
/// than carried into a scope that has no repository.
#[tokio::test]
async fn a_nested_note_is_promoted_to_the_top_of_the_global_scope() {
    let f = Fixture::with_global_scope().await;
    write(&f.memory("topics/user.md"), NOTE);

    let res = promote(&f.ctx, &f.memory("topics/user.md")).await.unwrap();

    assert_eq!(res.to, "global/user.md");
    assert!(f.memory("global/user.md").exists());
    let tombstone = f
        .server
        .pushes()
        .into_iter()
        .find(|p| p.deleted)
        .expect("the old path was never tombstoned");
    assert_eq!(
        tombstone.file_path, "topics/user.md",
        "the tombstone has to name the path the note actually had"
    );
}

#[tokio::test]
async fn promoting_with_global_sync_off_is_refused_and_moves_nothing() {
    let f = Fixture::new().await;
    write(&f.memory("user.md"), NOTE);

    let err = promote(&f.ctx, &f.memory("user.md")).await.unwrap_err();

    assert!(matches!(err, Error::GlobalNotConfigured), "{err:?}");
    assert_eq!(read(&f.memory("user.md")), NOTE);
    assert!(
        !f.memory("global/user.md").exists(),
        "a global copy was written with no global scope to sync it"
    );
    assert!(f.server.pushes().is_empty(), "{:?}", f.server.pushes());
}

#[tokio::test]
async fn a_path_that_is_not_a_memory_file_of_this_project_is_refused() {
    let f = Fixture::with_global_scope().await;
    write(&f.memory("topics/auth.md"), "auth\n");
    let outside = f.dir.path().join("elsewhere.md");
    std::fs::write(&outside, "not memory\n").unwrap();

    for path in [
        outside.clone(),
        // The memory directory itself, and a directory inside it: both are
        // inside the tree and neither is a note.
        f.ctx.memory_dir.clone(),
        f.memory("topics"),
    ] {
        let err = promote(&f.ctx, &path).await.unwrap_err();
        assert!(
            matches!(err, Error::NotAMemoryFile { .. }),
            "for {}: {err:?}",
            path.display()
        );
    }

    assert_eq!(read(&outside), "not memory\n");
    assert!(f.server.pushes().is_empty(), "{:?}", f.server.pushes());
}

#[tokio::test]
async fn a_note_that_is_already_global_is_refused() {
    let f = Fixture::with_global_scope().await;
    write(&f.memory("global/user.md"), NOTE);

    let err = promote(&f.ctx, &f.memory("global/user.md"))
        .await
        .unwrap_err();

    assert!(matches!(err, Error::AlreadyGlobal { .. }), "{err:?}");
    assert_eq!(read(&f.memory("global/user.md")), NOTE);
    assert!(f.server.pushes().is_empty(), "{:?}", f.server.pushes());
}

#[tokio::test]
async fn a_note_that_does_not_exist_is_refused() {
    let f = Fixture::with_global_scope().await;

    let err = promote(&f.ctx, &f.memory("never-written.md"))
        .await
        .unwrap_err();

    assert!(matches!(err, Error::Missing { .. }), "{err:?}");
    assert!(f.server.pushes().is_empty(), "{:?}", f.server.pushes());
}

/// Promoting the index would look like it worked and then undo itself: the
/// next push or pull regenerates `MEMORY.md` here, while a copy of this
/// repository's index sits in every other project.
#[tokio::test]
async fn the_projects_index_cannot_be_promoted() {
    let f = Fixture::with_global_scope().await;
    write(&f.memory("MEMORY.md"), "# Memory\n");

    let err = promote(&f.ctx, &f.memory("MEMORY.md")).await.unwrap_err();

    assert!(matches!(err, Error::IsTheIndex), "{err:?}");
    assert_eq!(read(&f.memory("MEMORY.md")), "# Memory\n");
    assert!(f.server.pushes().is_empty(), "{:?}", f.server.pushes());
}

/// Overwriting would destroy a note the user chose to carry into every
/// project, with no local copy left to recover it from. Both notes survive
/// the refusal, and the user's way out is one rename.
#[tokio::test]
async fn a_name_already_taken_in_the_global_scope_is_refused_rather_than_overwritten() {
    let f = Fixture::with_global_scope().await;
    write(&f.memory("global/user.md"), "the global note\n");
    write(&f.memory("user.md"), "this project's note\n");

    let err = promote(&f.ctx, &f.memory("user.md")).await.unwrap_err();

    assert!(matches!(err, Error::NameTaken { .. }), "{err:?}");
    assert_eq!(read(&f.memory("global/user.md")), "the global note\n");
    assert_eq!(read(&f.memory("user.md")), "this project's note\n");
    assert!(f.server.pushes().is_empty(), "{:?}", f.server.pushes());
}

/// What an interrupted promotion leaves behind: the global copy written, the
/// project copy not yet removed, no tombstone sent. Running it again has to
/// finish the move — refusing it as a name collision would strand the user
/// with a duplicate they can only resolve by hand.
#[tokio::test]
async fn promoting_again_after_an_interrupted_run_finishes_the_move() {
    let f = Fixture::with_global_scope().await;
    write(&f.memory("global/user.md"), NOTE);
    write(&f.memory("user.md"), NOTE);

    let res = promote(&f.ctx, &f.memory("user.md")).await.unwrap();

    assert!(res.resumed, "{res:?}");
    assert_eq!(res.to, "global/user.md");
    assert!(
        !f.memory("user.md").exists(),
        "the duplicate is still there"
    );
    assert_eq!(read(&f.memory("global/user.md")), NOTE);
    let tombstone = f
        .server
        .pushes()
        .into_iter()
        .find(|p| p.deleted)
        .expect("the interrupted run's missing tombstone was never sent");
    assert_eq!(tombstone.project_key, "acme/app");
    assert_eq!(tombstone.file_path, "user.md");
}

/// A note that cannot cross the wire must be refused before the move, not
/// after: the tombstone would take it out of the project scope and nothing
/// would ever have stored it in the global one.
#[tokio::test]
async fn a_note_that_is_not_valid_utf8_is_refused_before_anything_moves() {
    let f = Fixture::with_global_scope().await;
    write(&f.memory("user.md"), "");
    std::fs::write(f.memory("user.md"), [0xff, 0xfe, 0x00, 0x9f]).unwrap();

    let err = promote(&f.ctx, &f.memory("user.md")).await.unwrap_err();

    assert!(
        matches!(err, Error::Sync(SyncError::NotUtf8 { .. })),
        "{err:?}"
    );
    assert!(f.memory("user.md").exists());
    assert!(!f.memory("global/user.md").exists());
    assert!(f.server.pushes().is_empty(), "{:?}", f.server.pushes());
}

/// The property the whole ordering exists for: whatever fails, the note is
/// never at neither path. The server refusing the store is the failure a
/// user will actually hit — an expired token, a server that is down.
#[tokio::test]
async fn a_server_that_refuses_the_store_leaves_the_note_exactly_where_it_was() {
    let f = Fixture::with_global_scope().await;
    write(&f.memory("user.md"), NOTE);
    f.server.fail_with(500, "boom");

    let err = promote(&f.ctx, &f.memory("user.md")).await.unwrap_err();

    assert!(
        matches!(err, Error::Sync(SyncError::Push { .. })),
        "{err:?}"
    );
    assert_eq!(
        read(&f.memory("user.md")),
        NOTE,
        "the note was moved for a store that never landed"
    );
    assert!(
        !f.memory("global/user.md").exists(),
        "a global copy was left behind for a store the server refused"
    );
}
