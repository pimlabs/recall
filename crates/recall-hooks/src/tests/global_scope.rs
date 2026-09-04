//! Memories that follow the user into every project.
//!
//! The thing worth protecting here is the routing: a global note must never
//! land in one repository's history, and a project note must never leak into
//! every other project. Both are one-way doors once they reach the server.

use recall_wire::File;

use super::{write, Fixture};
use crate::pull::pull;
use crate::push::push;
use crate::state;

#[tokio::test]
async fn a_file_under_global_is_pushed_to_the_global_key() {
    let f = Fixture::with_global_scope().await;
    write(&f.memory("global/editor.md"), "prefers helix\n");

    let res = push(&f.ctx, &f.memory("global/editor.md")).await.unwrap();
    assert_eq!(res.pushed.as_deref(), Some("global/editor.md"));

    let pushes = f.server.pushes();
    let sent = pushes
        .iter()
        .find(|p| p.file_path == "editor.md")
        .unwrap_or_else(|| panic!("global file was not sent stripped of its prefix: {pushes:?}"));
    assert_eq!(sent.project_key, "global:eko");
    assert_eq!(sent.content.as_deref(), Some("prefers helix\n"));
}

#[tokio::test]
async fn a_file_outside_global_still_goes_to_the_project() {
    let f = Fixture::with_global_scope().await;
    write(&f.memory("MEMORY.md"), "# Memory\n");

    push(&f.ctx, &f.memory("MEMORY.md")).await.unwrap();

    let sent = f
        .server
        .pushes()
        .into_iter()
        .find(|p| p.file_path == "MEMORY.md")
        .expect("project file was not sent");
    assert_eq!(sent.project_key, "acme/app");
}

/// The one-way door. With global sync off, a leftover `global/` directory
/// must not be swept into this repository's history.
#[tokio::test]
async fn with_global_off_a_leftover_global_directory_is_not_pushed_anywhere() {
    let f = Fixture::new().await;
    write(&f.memory("global/editor.md"), "private preferences\n");

    let res = push(&f.ctx, &f.memory("global/editor.md")).await.unwrap();

    assert_eq!(res.pushed, None, "a global file was pushed with global off");
    assert!(
        f.server.pushes().is_empty(),
        "nothing at all should have been sent: {:?}",
        f.server.pushes()
    );
}

#[tokio::test]
async fn pull_writes_each_scope_into_its_own_subtree() {
    let f = Fixture::with_global_scope().await;
    f.server.set_files_for(
        "acme/app",
        vec![File {
            file_path: "MEMORY.md".into(),
            content: Some("# Project\n".into()),
            ..Default::default()
        }],
    );
    f.server.set_files_for(
        "global:eko",
        vec![File {
            file_path: "editor.md".into(),
            content: Some("prefers helix\n".into()),
            ..Default::default()
        }],
    );

    let res = pull(&f.ctx).await.unwrap();

    // The project's own content is written verbatim; the link to the global
    // file is the one thing Recall adds, and only because global sync is on.
    let memory_md = std::fs::read_to_string(f.memory("MEMORY.md")).unwrap();
    assert!(memory_md.starts_with("# Project\n"), "{memory_md:?}");
    assert!(memory_md.contains("](global/editor.md)"), "{memory_md:?}");

    assert_eq!(
        std::fs::read_to_string(f.memory("global/editor.md")).unwrap(),
        "prefers helix\n"
    );
    assert!(
        res.written.contains(&"global/editor.md".to_string()),
        "{res:?}"
    );
}

/// Without this, the files are on disk and Claude Code never reads them —
/// which is the entire failure mode this feature exists to avoid.
#[tokio::test]
async fn pull_leaves_the_global_files_reachable_from_memory_md() {
    let f = Fixture::with_global_scope().await;
    f.server.set_files_for(
        "global:eko",
        vec![File {
            file_path: "editor.md".into(),
            content: Some("---\ndescription: \"Prefers helix\"\n---\n\nbody\n".into()),
            ..Default::default()
        }],
    );

    pull(&f.ctx).await.unwrap();

    // Directly, not through an index file: an indirection measured
    // unreliable — see the module docs on `index`.
    let memory_md = std::fs::read_to_string(f.memory("MEMORY.md")).unwrap();
    assert!(
        memory_md.contains("- [editor](global/editor.md) — Prefers helix"),
        "MEMORY.md must link the file itself, or it is never read:\n{memory_md}"
    );
}

/// Recall edits MEMORY.md to add the links. That edit must not itself start
/// an endless push loop: the file is only rewritten when its content would
/// actually change.
#[tokio::test]
async fn refreshing_the_links_is_idempotent() {
    let f = Fixture::with_global_scope().await;
    write(&f.memory("global/editor.md"), "x\n");

    push(&f.ctx, &f.memory("global/editor.md")).await.unwrap();
    let after_first = std::fs::read_to_string(f.memory("MEMORY.md")).unwrap();
    assert!(after_first.contains("](global/editor.md)"), "{after_first}");

    push(&f.ctx, &f.memory("global/editor.md")).await.unwrap();
    assert_eq!(
        after_first,
        std::fs::read_to_string(f.memory("MEMORY.md")).unwrap(),
        "MEMORY.md was rewritten with identical content"
    );
}

/// A delete has to be reported to the scope that held the file, or the
/// tombstone lands under the wrong key and the file comes back on the next
/// pull.
#[tokio::test]
async fn a_deleted_global_file_is_tombstoned_under_the_global_key() {
    let f = Fixture::with_global_scope().await;
    write(&f.memory("global/editor.md"), "x\n");
    write(&f.memory("MEMORY.md"), "# Memory\n");
    push(&f.ctx, &f.memory("MEMORY.md")).await.unwrap();

    let baseline = state::load(&f.ctx.state_file).unwrap().unwrap();
    assert!(
        baseline.files.contains(&"global/editor.md".to_string()),
        "the baseline should have recorded the global file: {baseline:?}"
    );

    std::fs::remove_file(f.memory("global/editor.md")).unwrap();
    let res = push(&f.ctx, &f.memory("MEMORY.md")).await.unwrap();

    assert_eq!(res.deleted, vec!["global/editor.md".to_string()]);
    let tombstone = f
        .server
        .pushes()
        .into_iter()
        .find(|p| p.deleted)
        .expect("no tombstone was sent");
    assert_eq!(tombstone.project_key, "global:eko");
    assert_eq!(tombstone.file_path, "editor.md");
}

/// Nesting inside the global scope has to survive both directions, or a
/// tidily-organised set of preferences round-trips into a flat one.
#[tokio::test]
async fn nested_global_paths_round_trip() {
    let f = Fixture::with_global_scope().await;
    write(&f.memory("global/prefs/editor.md"), "helix\n");

    push(&f.ctx, &f.memory("global/prefs/editor.md"))
        .await
        .unwrap();
    let sent = f
        .server
        .pushes()
        .into_iter()
        .find(|p| !p.deleted)
        .expect("nothing sent");
    assert_eq!(sent.project_key, "global:eko");
    assert_eq!(sent.file_path, "prefs/editor.md");

    // And back the other way.
    let g = Fixture::with_global_scope().await;
    g.server.set_files_for(
        "global:eko",
        vec![File {
            file_path: "prefs/editor.md".into(),
            content: Some("helix\n".into()),
            ..Default::default()
        }],
    );
    pull(&g.ctx).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(g.memory("global/prefs/editor.md")).unwrap(),
        "helix\n"
    );
}

/// With global off, behaviour must be exactly what it was before scopes
/// existed — no index, no extra line in MEMORY.md, no second request.
#[tokio::test]
async fn global_off_changes_nothing_at_all() {
    let f = Fixture::new().await;
    f.server.set_files_for(
        "acme/app",
        vec![File {
            file_path: "MEMORY.md".into(),
            content: Some("# Project\n".into()),
            ..Default::default()
        }],
    );

    pull(&f.ctx).await.unwrap();

    assert_eq!(
        std::fs::read_to_string(f.memory("MEMORY.md")).unwrap(),
        "# Project\n",
        "MEMORY.md was modified with global sync off"
    );
    assert!(
        !std::fs::read_to_string(f.memory("MEMORY.md"))
            .unwrap()
            .contains("global/"),
        "a global link appeared with global sync off"
    );
    assert_eq!(
        f.server.pulled_keys(),
        vec!["acme/app".to_string()],
        "a second key was fetched with global sync off"
    );
}
