//! The base a push names: the version this machine's edit started from.
//!
//! The server merges only when what it holds is not that version, so these
//! pin that the base is the right one in every way a file reaches this
//! machine — pulled, pushed, promoted — and that it is absent rather than
//! wrong whenever it cannot be known.

use std::fs;

use recall_wire::{content_sha256, File};

use super::{write, Fixture};
use crate::promote::{promote, Target};
use crate::pull::pull;
use crate::push::push;
use crate::state;

fn file(path: &str, content: &str) -> File {
    File {
        file_path: path.into(),
        content: Some(content.into()),
        ..Default::default()
    }
}

/// The ordinary cycle: a session starts, pulls, and edits what it pulled.
/// The edit's base is the pulled version, so the server — which still holds
/// exactly that — lets the edit replace it, deletions included.
#[tokio::test]
async fn a_push_after_a_pull_names_the_pulled_version() {
    let f = Fixture::new().await;
    f.server
        .set_files(vec![file("MEMORY.md", "v1\nstale line\n")]);
    pull(&f.ctx).await.unwrap();

    write(&f.memory("MEMORY.md"), "v1\n");
    push(&f.ctx, &f.memory("MEMORY.md")).await.unwrap();

    let pushes = f.server.pushes();
    assert_eq!(
        pushes[0].base_sha256.as_deref(),
        Some(content_sha256("v1\nstale line\n").as_str())
    );
}

/// The second edit of a session starts from the first, not from the pull.
#[tokio::test]
async fn the_next_push_names_what_was_sent_last() {
    let f = Fixture::new().await;
    f.server.set_files(vec![file("MEMORY.md", "v1\n")]);
    pull(&f.ctx).await.unwrap();

    write(&f.memory("MEMORY.md"), "v2\n");
    push(&f.ctx, &f.memory("MEMORY.md")).await.unwrap();
    write(&f.memory("MEMORY.md"), "v3\n");
    push(&f.ctx, &f.memory("MEMORY.md")).await.unwrap();

    let pushes = f.server.pushes();
    assert_eq!(
        pushes[1].base_sha256.as_deref(),
        Some(content_sha256("v2\n").as_str()),
        "the base is what this machine sent — after a merge the server holds \
         something else, and naming that would let this push overwrite the \
         other side's changes"
    );
}

/// No base is better than a wrong one: without it the server merges, which
/// is what it always did.
#[tokio::test]
async fn a_file_never_synced_here_has_no_base() {
    let f = Fixture::new().await;
    write(&f.memory("new.md"), "fresh\n");
    push(&f.ctx, &f.memory("new.md")).await.unwrap();
    assert_eq!(f.server.pushes()[0].base_sha256, None);
}

/// A baseline written before bases existed is still a baseline, and its
/// files simply have none.
#[tokio::test]
async fn a_baseline_from_before_bases_still_loads() {
    let f = Fixture::new().await;
    fs::create_dir_all(f.ctx.state_file.parent().unwrap()).unwrap();
    fs::write(&f.ctx.state_file, r#"{"files":["MEMORY.md"]}"#).unwrap();

    let loaded = state::load(&f.ctx.state_file).unwrap().unwrap();
    assert_eq!(loaded.files, ["MEMORY.md"]);
    assert!(loaded.bases.is_empty());

    write(&f.memory("MEMORY.md"), "edited\n");
    push(&f.ctx, &f.memory("MEMORY.md")).await.unwrap();
    assert_eq!(f.server.pushes()[0].base_sha256, None);
}

/// A file deleted here gives up its base, so one recreated later at the same
/// path does not claim to be an edit of a version it never saw.
#[tokio::test]
async fn a_deleted_file_loses_its_base() {
    let f = Fixture::new().await;
    f.server
        .set_files(vec![file("MEMORY.md", "m\n"), file("gone.md", "g\n")]);
    pull(&f.ctx).await.unwrap();
    assert!(state::load(&f.ctx.state_file)
        .unwrap()
        .unwrap()
        .bases
        .contains_key("gone.md"));

    fs::remove_file(f.memory("gone.md")).unwrap();
    write(&f.memory("MEMORY.md"), "m2\n");
    push(&f.ctx, &f.memory("MEMORY.md")).await.unwrap();

    let bases = state::load(&f.ctx.state_file).unwrap().unwrap().bases;
    assert!(!bases.contains_key("gone.md"), "{bases:?}");
    assert_eq!(bases.get("MEMORY.md"), Some(&content_sha256("m2\n")));
}

/// Promotion pushes `MEMORY.md` to *remove* a line. Merged against the
/// stored index, which still has that line, the merge would keep it — so
/// this push is the one that most needs its base.
#[tokio::test]
async fn the_index_a_promotion_rewrites_carries_its_base() {
    let f = Fixture::with_global_scope().await;
    let index = "# Memory\n\n- [Commit style](topics/user.md)\n";
    f.server.set_files_for(
        "acme/app",
        vec![
            file("MEMORY.md", index),
            file("topics/user.md", "---\nname: user\n---\nbody\n"),
        ],
    );
    pull(&f.ctx).await.unwrap();

    promote(&f.ctx, &f.memory("topics/user.md"), Target::Global)
        .await
        .unwrap();

    let pushes = f.server.pushes();
    let pushed_index = pushes
        .iter()
        .find(|p| p.file_path == "MEMORY.md")
        .expect("the index was pushed");
    // The version the pull wrote — which is what the server still holds,
    // even though the index was regenerated on disk afterwards.
    assert_eq!(
        pushed_index.base_sha256.as_deref(),
        Some(content_sha256(index).as_str()),
        "without the base the merge keeps the stored index's line, and the \
         dead link comes back"
    );
    let after = state::load(&f.ctx.state_file).unwrap().unwrap();
    assert_eq!(
        after.bases.get("MEMORY.md"),
        Some(&content_sha256(pushed_index.content.as_deref().unwrap())),
        "and the next base is what was sent"
    );
}
