//! What a push and a pull do, end to end, against a real HTTP server.
//!
//! These drive [`push`](crate::push) and [`pull`](crate::pull) through a
//! real local server rather than a mock, so the JSON on the wire is the JSON
//! the deployed server would see.

use std::fs;
use std::path::PathBuf;

use recall_wire::File;

use super::{write, Fixture};
use crate::pull::{pull, PullOutcome};
use crate::push::push;
use crate::{state, Error};

#[tokio::test]
async fn push_ignores_files_outside_the_memory_dir() {
    let f = Fixture::new().await;
    let other = f.dir.path().join("src").join("main.rs");
    write(&other, "fn main() {}");

    let res = push(&f.ctx, &other).await.unwrap();
    assert!(res.skipped, "a non-memory file should be skipped");
    assert!(f.server.pushes().is_empty());
    // A skipped run must not write a baseline either — doing so on an
    // unrelated edit would record an empty directory as the truth.
    assert!(
        !f.ctx.state_file.exists(),
        "state file written for a skipped push"
    );
}

/// `/a/memory-notes` must not count as inside `/a/memory`.
#[tokio::test]
async fn push_ignores_a_sibling_directory_sharing_a_name_prefix() {
    let f = Fixture::new().await;
    let mut sneaky = f.ctx.memory_dir.clone().into_os_string();
    sneaky.push("-notes");
    let sneaky = PathBuf::from(sneaky).join("NOTES.md");
    write(&sneaky, "not memory");

    let res = push(&f.ctx, &sneaky).await.unwrap();
    assert!(
        res.skipped,
        "a sibling directory sharing a name prefix was treated as inside the memory dir"
    );
    assert!(f.server.pushes().is_empty());
}

#[tokio::test]
async fn push_sends_exact_bytes() {
    for (name, content) in [
        ("no trailing newline", "# Memory\n- fact"),
        ("one trailing newline", "# Memory\n- fact\n"),
        ("two trailing newlines", "# Memory\n- fact\n\n"),
        ("empty file", ""),
        ("crlf line endings", "# Memory\r\n- fact\r\n"),
        ("trailing spaces preserved", "# Memory\n- fact   \n"),
        ("leading and trailing blank lines", "\n\n# Memory\n\n\n"),
        ("tabs and unicode", "\t# Mémoire — ✅\n"),
    ] {
        let f = Fixture::new().await;
        let path = f.memory("MEMORY.md");
        write(&path, content);

        push(&f.ctx, &path).await.unwrap();

        let pushes = f.server.pushes();
        assert_eq!(pushes.len(), 1, "{name}: expected exactly one push");
        assert_eq!(
            pushes[0].content.as_deref(),
            Some(content),
            "{name}: content round trip altered the bytes"
        );
        assert!(!pushes[0].deleted, "{name}: a content push is not a delete");
    }
}

/// An empty memory file is a legitimate push, not a tombstone.
#[tokio::test]
async fn push_of_an_empty_file_is_not_a_delete() {
    let f = Fixture::new().await;
    let path = f.memory("EMPTY.md");
    write(&path, "");
    push(&f.ctx, &path).await.unwrap();

    let pushes = f.server.pushes();
    assert_eq!(pushes.len(), 1);
    assert!(!pushes[0].deleted);
    assert_eq!(pushes[0].content.as_deref(), Some(""));
}

/// A file Claude names on the fly is pushed exactly like `MEMORY.md` —
/// there is no list of known filenames anywhere in this path.
#[tokio::test]
async fn push_catches_a_dynamically_named_topic_file() {
    let f = Fixture::new().await;
    let path = f.memory("debugging.md");
    write(&path, "# Debugging\n");

    let res = push(&f.ctx, &path).await.unwrap();
    assert_eq!(res.pushed.as_deref(), Some("debugging.md"));
}

#[tokio::test]
async fn push_sends_nested_paths_with_forward_slashes() {
    let f = Fixture::new().await;
    let path = f.memory("topics/auth.md");
    write(&path, "# Auth\n");

    push(&f.ctx, &path).await.unwrap();
    assert_eq!(f.server.pushes()[0].file_path, "topics/auth.md");
}

#[tokio::test]
async fn push_sends_the_project_key_and_source_env() {
    let f = Fixture::new().await;
    let path = f.memory("MEMORY.md");
    write(&path, "x");
    push(&f.ctx, &path).await.unwrap();

    let pushed = &f.server.pushes()[0];
    assert_eq!(pushed.project_key, "acme/app");
    assert_eq!(pushed.source_env, "test");
}

/// The single most dangerous failure mode: on a first run there is no
/// baseline, and an empty or partial memory directory must not be read as
/// "everything was deleted".
#[tokio::test]
async fn push_does_not_tombstone_everything_on_the_first_run() {
    let f = Fixture::new().await;
    let path = f.memory("MEMORY.md");
    write(&path, "# Memory\n");

    let res = push(&f.ctx, &path).await.unwrap();
    assert!(res.deleted.is_empty(), "first run reported deletes");
    assert!(
        !f.server.pushes().iter().any(|p| p.deleted),
        "first run sent a tombstone"
    );
}

/// The same guard, in the shape that actually bites: a fresh clone whose
/// memory directory holds only the file being edited, while the server
/// holds a hundred more.
#[tokio::test]
async fn push_does_not_tombstone_on_a_fresh_clone_with_a_partial_memory_dir() {
    let f = Fixture::new().await;
    let path = f.memory("MEMORY.md");
    write(&path, "# Memory\n");
    assert!(
        state::load(&f.ctx.state_file).unwrap().is_none(),
        "precondition: no baseline"
    );

    let res = push(&f.ctx, &path).await.unwrap();
    assert!(res.deleted.is_empty());
    assert_eq!(f.server.pushes().len(), 1, "only the edited file was sent");
}

#[tokio::test]
async fn push_reconciles_deletes() {
    let f = Fixture::new().await;
    let memory = f.memory("MEMORY.md");
    let gone = f.memory("gone.md");
    write(&memory, "# Memory\n");
    write(&gone, "# Gone\n");

    // Establish a baseline containing both.
    push(&f.ctx, &memory).await.unwrap();
    fs::remove_file(&gone).unwrap();

    // A later edit to an unrelated memory file is what carries the delete.
    let res = push(&f.ctx, &memory).await.unwrap();
    assert_eq!(res.deleted, vec!["gone.md"]);
    assert!(
        f.server
            .pushes()
            .iter()
            .any(|p| p.deleted && p.file_path == "gone.md"),
        "no tombstone push was sent for the deleted file"
    );

    // And the baseline no longer lists it, so it isn't re-reported forever.
    let baseline = state::load(&f.ctx.state_file).unwrap().expect("a baseline");
    assert!(
        !baseline.files.iter().any(|f| f == "gone.md"),
        "deleted file still listed in the refreshed baseline"
    );

    // A third run has nothing left to reconcile.
    let res = push(&f.ctx, &memory).await.unwrap();
    assert!(res.deleted.is_empty(), "delete re-reported on a later run");
}

#[tokio::test]
async fn push_reconciles_a_nested_delete() {
    let f = Fixture::new().await;
    let memory = f.memory("MEMORY.md");
    let nested = f.memory("topics/auth.md");
    write(&memory, "# Memory\n");
    write(&nested, "# Auth\n");

    push(&f.ctx, &memory).await.unwrap();
    fs::remove_file(&nested).unwrap();

    let res = push(&f.ctx, &memory).await.unwrap();
    assert_eq!(res.deleted, vec!["topics/auth.md"]);
}

#[tokio::test]
async fn push_surfaces_a_server_rejection() {
    let f = Fixture::new().await;
    let path = f.memory("MEMORY.md");
    write(&path, "# Memory\n");
    f.server.fail_with(401, r#"{"error":"unauthorized"}"#);

    let err = push(&f.ctx, &path).await.unwrap_err();
    assert!(
        matches!(&err, Error::Push { path, .. } if path == "MEMORY.md"),
        "got {err:?}"
    );
    assert!(err.to_string().contains("401"), "{err}");
}

#[tokio::test]
async fn push_refuses_a_memory_file_that_is_not_utf8() {
    let f = Fixture::new().await;
    let path = f.memory("binary.md");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();

    let err = push(&f.ctx, &path).await.unwrap_err();
    assert!(matches!(err, Error::NotUtf8 { .. }), "got {err:?}");
    assert!(
        f.server.pushes().is_empty(),
        "corrupted bytes must not reach the server"
    );
}

#[tokio::test]
async fn pull_writes_files_and_removes_tombstoned() {
    let f = Fixture::new().await;
    let content = "# Memory\n- from the server\n";
    let stale = f.memory("stale.md");
    write(&stale, "should be removed");

    f.server.set_files(vec![
        File {
            file_path: "MEMORY.md".into(),
            content: Some(content.into()),
            ..Default::default()
        },
        File {
            file_path: "stale.md".into(),
            content: None,
            deleted: true,
            ..Default::default()
        },
    ]);

    let res = pull(&f.ctx).await.unwrap();
    assert_eq!(res.written, vec!["MEMORY.md"]);
    assert_eq!(res.removed, vec!["stale.md"]);
    assert_eq!(fs::read_to_string(f.memory("MEMORY.md")).unwrap(), content);
    assert!(!stale.exists(), "tombstoned file still present after pull");
}

#[tokio::test]
async fn pull_creates_nested_directories() {
    let f = Fixture::new().await;
    f.server.set_files(vec![File {
        file_path: "topics/deep/auth.md".into(),
        content: Some("# Auth\n".into()),
        ..Default::default()
    }]);

    let res = pull(&f.ctx).await.unwrap();
    assert_eq!(res.written, vec!["topics/deep/auth.md"]);
    assert_eq!(
        fs::read_to_string(f.memory("topics/deep/auth.md")).unwrap(),
        "# Auth\n"
    );
}

/// A tombstone for a file that was never here is not an error.
#[tokio::test]
async fn pull_tolerates_a_tombstone_for_a_missing_file() {
    let f = Fixture::new().await;
    f.server.set_files(vec![File {
        file_path: "never-existed.md".into(),
        content: None,
        deleted: true,
        ..Default::default()
    }]);

    let res = pull(&f.ctx).await.unwrap();
    assert!(res.removed.is_empty());
}

/// A malicious or buggy server must not be able to make a pull write
/// outside the memory directory.
#[tokio::test]
async fn pull_refuses_traversal_paths() {
    let mut f = Fixture::new().await;
    // Nested two deep so that even `../../` lands inside the temp
    // directory the test owns, and the assertions below are about real
    // paths rather than about the machine's temp root.
    f.ctx.memory_dir = f.dir.path().join("a").join("b").join("memory");
    let outside_root = f.dir.path().to_path_buf();
    f.server.set_files(
        [
            "../../escaped.md",
            "../escaped.md",
            "/etc/passwd",
            "topics/../../escaped.md",
            r"..\escaped.md",
            "C:/Windows/system32/evil.md",
            "",
        ]
        .into_iter()
        .map(|p| File {
            file_path: p.into(),
            content: Some("pwned".into()),
            ..Default::default()
        })
        .collect(),
    );

    let res = pull(&f.ctx).await.unwrap();
    assert!(res.written.is_empty(), "wrote {:?}", res.written);
    for escaped in [
        outside_root.join("a").join("b").join("escaped.md"),
        outside_root.join("a").join("escaped.md"),
        outside_root.join("escaped.md"),
    ] {
        assert!(
            !escaped.exists(),
            "a file was written outside the memory directory: {}",
            escaped.display()
        );
    }
    assert!(
        state::list_memory_files(&f.ctx.memory_dir)
            .unwrap()
            .is_empty(),
        "a refused path still landed inside the memory directory"
    );
}

/// One poisoned row must not stop the legitimate ones landing.
#[tokio::test]
async fn pull_still_writes_good_files_alongside_a_refused_one() {
    let f = Fixture::new().await;
    f.server.set_files(vec![
        File {
            file_path: "../escaped.md".into(),
            content: Some("pwned".into()),
            ..Default::default()
        },
        File {
            file_path: "MEMORY.md".into(),
            content: Some("# Memory\n".into()),
            ..Default::default()
        },
    ]);

    let res = pull(&f.ctx).await.unwrap();
    assert_eq!(res.written, vec!["MEMORY.md"]);
}

#[tokio::test]
async fn pull_round_trips_bytes_exactly() {
    for content in [
        "",
        "no newline",
        "one\n",
        "two\n\n",
        "\n\n\n",
        "crlf\r\n\r\n",
        "trailing spaces   \n",
    ] {
        let f = Fixture::new().await;
        f.server.set_files(vec![File {
            file_path: "MEMORY.md".into(),
            content: Some(content.into()),
            ..Default::default()
        }]);

        pull(&f.ctx).await.unwrap();
        let got = fs::read_to_string(f.memory("MEMORY.md")).unwrap();
        assert_eq!(got, content, "pull altered content");
    }
}

/// A machine that only ever pulls still needs an accurate baseline, or
/// its first local delete would go unnoticed.
#[tokio::test]
async fn pull_refreshes_the_baseline() {
    let f = Fixture::new().await;
    f.server.set_files(vec![File {
        file_path: "MEMORY.md".into(),
        content: Some("# Memory\n".into()),
        ..Default::default()
    }]);

    pull(&f.ctx).await.unwrap();
    let baseline = state::load(&f.ctx.state_file)
        .unwrap()
        .expect("no baseline after pull");
    assert_eq!(baseline.files, vec!["MEMORY.md"]);
}

/// Pull then delete then push: the end-to-end path that makes a delete
/// visible on a machine that never pushed the file in the first place.
#[tokio::test]
async fn a_pulled_file_deleted_locally_is_tombstoned_on_the_next_push() {
    let f = Fixture::new().await;
    f.server.set_files(vec![
        File {
            file_path: "MEMORY.md".into(),
            content: Some("# Memory\n".into()),
            ..Default::default()
        },
        File {
            file_path: "obsolete.md".into(),
            content: Some("# Obsolete\n".into()),
            ..Default::default()
        },
    ]);
    pull(&f.ctx).await.unwrap();

    fs::remove_file(f.memory("obsolete.md")).unwrap();
    let res = push(&f.ctx, &f.memory("MEMORY.md")).await.unwrap();
    assert_eq!(res.deleted, vec!["obsolete.md"]);
}

#[tokio::test]
async fn pull_surfaces_a_server_rejection() {
    let f = Fixture::new().await;
    f.server.fail_with(500, "boom");
    let err = pull(&f.ctx).await.unwrap_err();
    assert!(matches!(err, Error::Pull { .. }), "got {err:?}");
    assert!(err.to_string().contains("500"), "{err}");
}

/// An empty response leaves the disk alone rather than inventing a
/// baseline out of whatever happens to be there.
#[tokio::test]
async fn pull_with_nothing_on_the_server_is_a_no_op() {
    let f = Fixture::new().await;
    let res = pull(&f.ctx).await.unwrap();
    assert_eq!(res, PullOutcome::default());
    assert!(!f.ctx.state_file.exists());
    assert!(!f.ctx.memory_dir.exists());
}

#[test]
fn describes_a_pull() {
    let res = PullOutcome {
        written: vec!["MEMORY.md".into()],
        removed: vec!["a.md".into(), "b.md".into()],
    };
    assert_eq!(
        res.describe("acme/app"),
        "recall-pull: synced 1 memory file(s), removed 2 deleted file(s) for acme/app"
    );
}
