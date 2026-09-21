//! The machine scope, at the level the index gate lives at.
//!
//! [`index::refresh`] has its own tests, and they pass: it has been driven
//! off `RESERVED_DIRS` since the machine scope's index gap was closed, so it
//! handles `machine/` exactly as it handles `global/`. What those tests
//! cannot see is whether anything *calls* it. `pull`, `push` and `backfill`
//! each decide for themselves, and each asked about the global scope alone —
//! correct only while the global scope was the only reserved one.
//!
//! So these run the three entry points with a machine scope and **no global
//! scope**, which is the one configuration where the two questions give
//! different answers. A fixture carrying both would pass against the bug.

use recall_wire::File;

use super::{write, Fixture};
use crate::backfill::backfill;
use crate::pull::pull;
use crate::push::push;

/// The shipped symptom, one level up from where it was fixed: the file
/// arrives, and Claude Code never opens it, because Claude Code opens what
/// `MEMORY.md` links and nothing else.
#[tokio::test]
async fn pull_leaves_the_machine_files_reachable_from_memory_md() {
    let f = Fixture::with_machine_scope().await;
    f.server.set_files_for(
        "machine:mbp",
        vec![File {
            file_path: "ram.md".into(),
            content: Some(
                "---\ndescription: \"8 GB, so no parallel builds\"\n---\n\nbody\n".into(),
            ),
            ..Default::default()
        }],
    );

    pull(&f.ctx).await.unwrap();

    assert!(
        f.memory("machine/ram.md").exists(),
        "the file itself has always arrived; that was never the gap"
    );

    let memory_md = std::fs::read_to_string(f.memory("MEMORY.md")).unwrap();
    assert!(
        memory_md.contains("- [ram](machine/ram.md) — 8 GB, so no parallel builds"),
        "MEMORY.md must link it, or the sync is decorative:\n{memory_md}"
    );
}

/// Pushing a machine note has to leave the index naming it too. Otherwise
/// the machine that wrote the note is the one machine that cannot read it
/// back — it has no reason to pull the file it already has.
#[tokio::test]
async fn pushing_a_machine_file_refreshes_the_index() {
    let f = Fixture::with_machine_scope().await;
    write(
        &f.memory("machine/ram.md"),
        "---\ndescription: \"8 GB\"\n---\n\nbody\n",
    );

    push(&f.ctx, &f.memory("machine/ram.md")).await.unwrap();

    let memory_md = std::fs::read_to_string(f.memory("MEMORY.md")).unwrap();
    assert!(
        memory_md.contains("](machine/ram.md)"),
        "the index must name what was just pushed:\n{memory_md}"
    );
}

/// `backfill` refreshes before it lists, so the index it sends is the one it
/// just wrote. With the gate asking the wrong question it sent a `MEMORY.md`
/// that named nothing under `machine/`, and every other machine pulled that
/// stale index over its own.
#[tokio::test]
async fn backfill_refreshes_the_index_before_sending_it() {
    let f = Fixture::with_machine_scope().await;
    write(
        &f.memory("machine/ram.md"),
        "---\ndescription: \"8 GB\"\n---\n\nbody\n",
    );

    backfill(&f.ctx).await.unwrap();

    let memory_md = std::fs::read_to_string(f.memory("MEMORY.md")).unwrap();
    assert!(
        memory_md.contains("](machine/ram.md)"),
        "on disk:\n{memory_md}"
    );

    let sent = f
        .server
        .pushes()
        .into_iter()
        .find(|p| p.file_path == "MEMORY.md")
        .expect("backfill sends the index");
    assert!(
        sent.content.unwrap_or_default().contains("machine/ram.md"),
        "and sends the refreshed one, not the stale one it found"
    );
}

/// The gate's original question, kept as a question: with no reserved scope
/// at all there is nothing to index, and `MEMORY.md` must be left exactly as
/// the user wrote it. Widening the gate must not turn it into "always".
#[tokio::test]
async fn with_no_reserved_scope_the_index_is_still_left_alone() {
    let f = Fixture::new().await;
    write(&f.memory("MEMORY.md"), "# Memory\n");
    write(&f.memory("note.md"), "body\n");

    push(&f.ctx, &f.memory("note.md")).await.unwrap();

    assert_eq!(
        std::fs::read_to_string(f.memory("MEMORY.md")).unwrap(),
        "# Memory\n",
        "with every reserved scope off, Recall has no business in this file"
    );
}
