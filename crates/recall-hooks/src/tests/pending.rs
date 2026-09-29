//! Changes the hooks never saw: a note written, appended to or removed
//! through the shell, which fires no `Edit|Write` hook.
//!
//! A pull runs at every session start, resume and compaction, and it used to
//! write the server's copy over every file. These pin that a pull now sends
//! such a change first and leaves the file alone, that a push sweeps it up
//! along with the file it was triggered by, and that none of it fires for
//! a file nobody touched, for `MEMORY.md`'s derived index lines, or on a
//! machine's very first sync.

use std::fs;

use recall_wire::{content_sha256, File};

use super::{write, Fixture};
use crate::pull::pull;
use crate::push::push;

fn file(path: &str, content: &str) -> File {
    File {
        file_path: path.into(),
        content: Some(content.into()),
        ..Default::default()
    }
}

/// The case that lost memory: a pull, an edit through the shell, and a
/// second pull (a compaction, say) against a server that still holds the
/// old version. Mutation: drop the `send_pending` call from `pull`.
#[tokio::test]
async fn a_pull_sends_a_shell_edit_instead_of_overwriting_it() {
    let f = Fixture::new().await;
    f.server.set_files(vec![file("notes.md", "v1\n")]);
    pull(&f.ctx).await.unwrap();

    fs::write(f.memory("notes.md"), "v1\nadded from the shell\n").unwrap();
    let res = pull(&f.ctx).await.unwrap();

    assert_eq!(res.sent, vec!["notes.md"]);
    assert!(!res.written.contains(&"notes.md".to_string()), "{res:?}");
    assert_eq!(
        fs::read_to_string(f.memory("notes.md")).unwrap(),
        "v1\nadded from the shell\n",
        "the pull overwrote an edit it had not sent"
    );
    let pushes = f.server.pushes();
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(
        pushes[0].content.as_deref(),
        Some("v1\nadded from the shell\n")
    );
    // Its base is the version it was edited from, so the server replaces
    // its copy, or merges if another machine moved it in between.
    assert_eq!(
        pushes[0].base_sha256.as_deref(),
        Some(content_sha256("v1\n").as_str())
    );
}

#[tokio::test]
async fn a_pull_sends_a_file_the_hooks_never_saw_created() {
    let f = Fixture::new().await;
    f.server.set_files(vec![file("MEMORY.md", "# Memory\n")]);
    pull(&f.ctx).await.unwrap();

    write(&f.memory("new.md"), "made with cat >\n");
    let res = pull(&f.ctx).await.unwrap();

    assert_eq!(res.sent, vec!["new.md"]);
    let pushes = f.server.pushes();
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(pushes[0].file_path, "new.md");
    assert_eq!(pushes[0].base_sha256, None, "never synced, so no base");
    assert!(f.memory("new.md").exists());
}

/// A file removed through the shell is a delete, not something for the
/// pull to bring back.
#[tokio::test]
async fn a_pull_sends_a_shell_delete_rather_than_restoring_the_file() {
    let f = Fixture::new().await;
    f.server
        .set_files(vec![file("MEMORY.md", "# Memory\n"), file("old.md", "x\n")]);
    pull(&f.ctx).await.unwrap();

    fs::remove_file(f.memory("old.md")).unwrap();
    let res = pull(&f.ctx).await.unwrap();

    assert_eq!(res.sent_deletes, vec!["old.md"]);
    let pushes = f.server.pushes();
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert!(pushes[0].deleted);
    assert!(!f.memory("old.md").exists(), "the pull restored a delete");
}

/// The ordinary case is untouched: a file nobody changed here takes the
/// server's newer version and sends nothing.
#[tokio::test]
async fn a_pull_still_updates_a_file_nobody_changed_here() {
    let f = Fixture::new().await;
    f.server.set_files(vec![file("notes.md", "v1\n")]);
    pull(&f.ctx).await.unwrap();

    f.server
        .set_files(vec![file("notes.md", "v2 from another machine\n")]);
    let res = pull(&f.ctx).await.unwrap();

    assert!(res.sent.is_empty(), "{res:?}");
    assert!(f.server.pushes().is_empty());
    assert_eq!(
        fs::read_to_string(f.memory("notes.md")).unwrap(),
        "v2 from another machine\n"
    );
}

/// `MEMORY.md`'s links into a reserved directory are rewritten on this
/// machine after every pull. A file that differs only there has nothing to
/// send, or every session start would push the index. Mutation: record a
/// plain hash, not the fingerprint, in `State::disk`.
#[tokio::test]
async fn index_lines_alone_are_not_a_change() {
    let f = Fixture::with_global_scope().await;
    f.server
        .set_files_for("acme/app", vec![file("MEMORY.md", "# Memory\n")]);
    f.server
        .set_files_for("global:eko", vec![file("editor.md", "vim\n")]);
    pull(&f.ctx).await.unwrap();
    let memory_md = fs::read_to_string(f.memory("MEMORY.md")).unwrap();
    assert!(memory_md.contains("](global/editor.md)"), "{memory_md}");

    let res = pull(&f.ctx).await.unwrap();

    assert!(res.sent.is_empty(), "{res:?}");
    assert!(f.server.pushes().is_empty(), "{:?}", f.server.pushes());

    // The harder case: a new global note written here adds its link to
    // `MEMORY.md` in a run that never synced `MEMORY.md` itself.
    write(&f.memory("global/shell.md"), "zsh\n");
    push(&f.ctx, &f.memory("global/shell.md")).await.unwrap();
    let memory_md = fs::read_to_string(f.memory("MEMORY.md")).unwrap();
    assert!(memory_md.contains("](global/shell.md)"), "{memory_md}");

    let res = pull(&f.ctx).await.unwrap();
    assert!(
        !res.sent.contains(&"MEMORY.md".to_string()),
        "an index-only change to MEMORY.md was sent: {res:?}"
    );
}

/// A real edit to `MEMORY.md` is still one, index lines or not.
#[tokio::test]
async fn an_edit_to_memory_md_beside_its_index_lines_is_sent() {
    let f = Fixture::with_global_scope().await;
    f.server
        .set_files_for("acme/app", vec![file("MEMORY.md", "# Memory\n")]);
    f.server
        .set_files_for("global:eko", vec![file("editor.md", "vim\n")]);
    pull(&f.ctx).await.unwrap();

    let memory_md = fs::read_to_string(f.memory("MEMORY.md")).unwrap();
    fs::write(
        f.memory("MEMORY.md"),
        format!("{memory_md}- [N](notes.md)\n"),
    )
    .unwrap();
    let res = pull(&f.ctx).await.unwrap();

    assert_eq!(res.sent, vec!["MEMORY.md"]);
}

/// The push hook sends the file it was triggered by, and also anything else
/// changed behind its back since the last sync.
#[tokio::test]
async fn a_push_sweeps_up_a_shell_edit_to_another_file() {
    let f = Fixture::new().await;
    f.server
        .set_files(vec![file("a.md", "a1\n"), file("b.md", "b1\n")]);
    pull(&f.ctx).await.unwrap();

    fs::write(f.memory("b.md"), "b2 from the shell\n").unwrap();
    write(&f.memory("a.md"), "a2\n");
    let res = push(&f.ctx, &f.memory("a.md")).await.unwrap();

    assert_eq!(res.pushed.as_deref(), Some("a.md"));
    assert_eq!(res.swept, vec!["b.md"]);
    let pushes = f.server.pushes();
    let b = pushes.iter().find(|p| p.file_path == "b.md").unwrap();
    assert_eq!(b.content.as_deref(), Some("b2 from the shell\n"));
    assert_eq!(
        b.base_sha256.as_deref(),
        Some(content_sha256("b1\n").as_str())
    );

    // And once sent, it is in step again: the next push sends only what
    // triggered it.
    write(&f.memory("a.md"), "a3\n");
    let res = push(&f.ctx, &f.memory("a.md")).await.unwrap();
    assert!(res.swept.is_empty(), "{res:?}");
}

/// A machine's first sync has no "since": the local directory is whatever
/// predates Recall, and a pull keeps doing what it always did. Sending it
/// is `recall backfill`'s job, which asks the server first.
#[tokio::test]
async fn the_first_pull_sends_nothing() {
    let f = Fixture::new().await;
    write(&f.memory("local.md"), "predates recall\n");
    f.server.set_files(vec![file("MEMORY.md", "# Memory\n")]);

    let res = pull(&f.ctx).await.unwrap();

    assert!(
        res.sent.is_empty() && res.sent_deletes.is_empty(),
        "{res:?}"
    );
    assert!(f.server.pushes().is_empty());
}

/// Recall's own temporary files are never memory.
#[tokio::test]
async fn a_temporary_file_is_never_sent() {
    let f = Fixture::new().await;
    f.server.set_files(vec![file("MEMORY.md", "# Memory\n")]);
    pull(&f.ctx).await.unwrap();

    write(&f.memory(".recall-abc123.tmp"), "half written\n");
    let res = pull(&f.ctx).await.unwrap();

    assert!(res.sent.is_empty(), "{res:?}");
}
