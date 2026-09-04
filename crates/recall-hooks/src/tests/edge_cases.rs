//! Edge cases for the client, chosen the same way as the server's: what
//! would actually lose a note, corrupt one, or wedge a session.

use recall_wire::File;

use super::{write, Fixture};
use crate::client::Client;
use crate::context::Context;
use crate::pull::pull;
use crate::push::push;
use crate::state;
use crate::testserver::FakeServer;

// -----------------------------------------------------------------
// Filesystem shapes the memory directory can actually take
// -----------------------------------------------------------------

/// A symlinked directory inside the memory dir must not be descended
/// into. Descending would follow it out of the tree, and a symlink
/// pointing at an ancestor would make the walk never terminate.
#[tokio::test]
async fn the_walk_does_not_descend_into_a_symlinked_directory() {
    let f = Fixture::new().await;
    write(&f.memory("MEMORY.md"), "# Memory\n");

    let outside = f.dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.md"), "should never be listed").unwrap();
    std::os::unix::fs::symlink(&outside, f.ctx.memory_dir.join("link")).unwrap();

    // And a loop back to the memory dir itself, which is what would hang
    // a walk that followed links.
    std::os::unix::fs::symlink(&f.ctx.memory_dir, f.ctx.memory_dir.join("loop")).unwrap();

    // Completing at all is half the assertion: a walk that followed the
    // "loop" link would recurse until the stack ran out.
    let listed = state::list_memory_files(&f.ctx.memory_dir).unwrap();

    assert!(
        !listed.iter().any(|p| p.contains("secret.md")),
        "the walk followed a symlink out of the memory dir: {listed:?}"
    );
    assert!(listed.contains(&"MEMORY.md".to_string()));
    // The links are seen — they are just not descended into. Asserting
    // this keeps the test from passing for the wrong reason, e.g. if the
    // walk stopped listing anything at all.
    assert!(
        listed.contains(&"link".to_string()) && listed.contains(&"loop".to_string()),
        "expected both symlinks listed as entries, got {listed:?}"
    );
}

/// Claude Code names topic files itself, and nothing stops it choosing a
/// name with a space or an emoji in it.
#[tokio::test]
async fn awkward_filenames_push_and_pull_intact() {
    for name in [
        "with space.md",
        "émoji-🚀.md",
        "..config.md",
        "dots...md",
        "UPPER.MD",
        "日本語.md",
    ] {
        let f = Fixture::new().await;
        write(&f.memory(name), "content\n");

        let res = push(&f.ctx, &f.memory(name)).await.unwrap();
        assert_eq!(
            res.pushed.as_deref(),
            Some(name),
            "{name:?} was not pushed under its own name"
        );
        assert_eq!(f.server.pushes()[0].file_path, name);
    }
}

#[tokio::test]
async fn deeply_nested_topic_files_keep_their_relative_path() {
    let f = Fixture::new().await;
    let rel = "topics/auth/oauth/tokens.md";
    write(&f.memory(rel), "# Tokens\n");

    let res = push(&f.ctx, &f.memory(rel)).await.unwrap();
    assert_eq!(res.pushed.as_deref(), Some(rel));
    assert_eq!(
        f.server.pushes()[0].file_path,
        rel,
        "nested paths must travel slash-separated, not with the platform separator"
    );
}

/// A directory inside the memory dir is not a memory file. Pushing one
/// would read a directory as content.
#[tokio::test]
async fn a_directory_is_never_pushed_as_a_file() {
    let f = Fixture::new().await;
    let dir = f.memory("notes.md");
    std::fs::create_dir_all(&dir).unwrap();

    let res = push(&f.ctx, &dir).await.unwrap();
    assert_eq!(res.pushed, None, "a directory was pushed as a memory file");
}

/// The hook fires after the tool ran, but the file can be gone by then —
/// a write immediately followed by a delete, for instance. That is a
/// no-op, not an error that surfaces in the user's session.
#[tokio::test]
async fn a_triggering_file_that_vanished_is_not_an_error() {
    let f = Fixture::new().await;
    write(&f.memory("MEMORY.md"), "# Memory\n");
    let res = push(&f.ctx, &f.memory("ghost.md")).await.unwrap();
    assert_eq!(res.pushed, None);
}

// -----------------------------------------------------------------
// The baseline, which is what makes deletes work at all
// -----------------------------------------------------------------

/// A corrupt baseline must not wedge every future push. Losing one
/// delete propagation is recoverable; a hook that fails on every edit
/// until someone deletes a file by hand is not.
#[tokio::test]
async fn a_corrupt_baseline_is_treated_as_absent_not_fatal() {
    let f = Fixture::new().await;
    std::fs::create_dir_all(f.ctx.state_file.parent().unwrap()).unwrap();
    std::fs::write(&f.ctx.state_file, b"{not json at all").unwrap();
    write(&f.memory("MEMORY.md"), "# Memory\n");

    let res = push(&f.ctx, &f.memory("MEMORY.md")).await.unwrap();
    assert_eq!(res.pushed.as_deref(), Some("MEMORY.md"));
    assert!(
        res.deleted.is_empty(),
        "an unreadable baseline must not be read as 'everything was deleted'"
    );
    // And it gets replaced with a usable one.
    assert!(state::load(&f.ctx.state_file).unwrap().is_some());
}

/// The dangerous shape of the same bug: a baseline that parses but lists
/// files that are no longer there, on a machine where the memory dir was
/// wiped. Every one of them is a real delete, and the code must report
/// them — this is the case the first-run guard must NOT swallow.
#[tokio::test]
async fn a_wiped_memory_dir_with_a_baseline_reports_every_delete() {
    let f = Fixture::new().await;
    for name in ["a.md", "b.md", "c.md"] {
        write(&f.memory(name), "content\n");
    }
    push(&f.ctx, &f.memory("a.md")).await.unwrap();

    for name in ["b.md", "c.md"] {
        std::fs::remove_file(f.memory(name)).unwrap();
    }
    let res = push(&f.ctx, &f.memory("a.md")).await.unwrap();

    let mut deleted = res.deleted.clone();
    deleted.sort();
    assert_eq!(deleted, vec!["b.md".to_string(), "c.md".to_string()]);
}

/// Two hooks can run at once — Claude Code writes several memory files
/// in a turn. The baseline is written temp-then-rename precisely so a
/// reader never sees a half-written one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_pushes_never_leave_a_corrupt_baseline() {
    let server = FakeServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let memory_dir = dir.path().join("memory");
    let state_file = dir.path().join(".recall-state.json");
    std::fs::create_dir_all(&memory_dir).unwrap();
    for i in 0..12 {
        std::fs::write(memory_dir.join(format!("f{i}.md")), format!("content {i}")).unwrap();
    }

    let mut tasks = Vec::new();
    for i in 0..12 {
        let ctx = Context {
            memory_dir: memory_dir.clone(),
            state_file: state_file.clone(),
            project_key: "acme/app".into(),
            source_env: "test".into(),
            client: Client::new(&server.url, "token").unwrap(),
        };
        let path = memory_dir.join(format!("f{i}.md"));
        tasks.push(tokio::spawn(async move { push(&ctx, &path).await.is_ok() }));
    }
    for t in tasks {
        assert!(t.await.unwrap(), "a concurrent push failed");
    }

    // Whichever writer landed last, the file must be complete and
    // parseable — never truncated.
    let loaded = state::load(&state_file).unwrap();
    assert!(
        loaded.is_some(),
        "baseline unreadable after concurrent writes"
    );
    assert_eq!(loaded.unwrap().files.len(), 12);

    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(".recall-state-")
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp files left behind: {leftovers:?}"
    );
}

// -----------------------------------------------------------------
// Pull
// -----------------------------------------------------------------

/// Pull overwrites in place, so it has to cope with what is already
/// there — including a file the user made read-only.
#[tokio::test]
async fn pull_replaces_a_read_only_file() {
    use std::os::unix::fs::PermissionsExt;

    let f = Fixture::new().await;
    let dest = f.memory("MEMORY.md");
    write(&dest, "old\n");
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o444)).unwrap();

    f.server.set_files(vec![File {
        file_path: "MEMORY.md".into(),
        content: Some("new\n".into()),
        ..Default::default()
    }]);

    // Rename-over succeeds on a read-only *file* because the permission
    // that matters belongs to the directory — which is a property of the
    // atomic write, not a coincidence: an `fs::write` here would fail for
    // a non-root user and silently skip files the user protected.
    //
    // Worth knowing when reading a green run: this suite is run as root
    // in CI and in the project's container, and root ignores mode bits,
    // so this particular test is only a real negative on a normal user
    // account. It still pins the behavior against a rewrite.
    pull(&f.ctx).await.unwrap();
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new\n");
}

/// A path the server hands back that would escape the memory directory
/// must be refused — this is the moment it would become a write
/// somewhere else on this machine.
#[tokio::test]
async fn pull_refuses_every_escaping_path_shape() {
    let f = Fixture::new().await;
    let escapes = [
        "../escaped.md",
        "../../escaped.md",
        "topics/../../escaped.md",
        "/etc/passwd",
        "/tmp/escaped.md",
        "..",
        "",
    ];
    f.server.set_files(
        escapes
            .iter()
            .map(|p| File {
                file_path: (*p).into(),
                content: Some("pwned".into()),
                ..Default::default()
            })
            .collect(),
    );

    let res = pull(&f.ctx).await.unwrap();
    assert!(res.written.is_empty(), "wrote {:?}", res.written);

    // Nothing landed anywhere above the memory dir either.
    for entry in std::fs::read_dir(f.dir.path()).unwrap() {
        let name = entry.unwrap().file_name();
        assert_ne!(name.to_string_lossy(), "escaped.md");
    }
    assert!(!std::path::Path::new("/tmp/escaped.md").exists());
}

/// Unicode has to survive the client half too, not just the server's.
#[tokio::test]
async fn pull_writes_unicode_content_byte_for_byte() {
    let f = Fixture::new().await;
    let content = "# 日本語 🚀\n- combining: e\u{0301}\n- nul:\u{0}end\n";
    f.server.set_files(vec![File {
        file_path: "unicode.md".into(),
        content: Some(content.into()),
        ..Default::default()
    }]);

    pull(&f.ctx).await.unwrap();
    assert_eq!(
        std::fs::read(f.memory("unicode.md")).unwrap(),
        content.as_bytes()
    );
}

/// A tombstone for a file this machine never had is not an error, and
/// must not be counted as a removal.
#[tokio::test]
async fn pull_tolerates_a_tombstone_for_a_file_that_is_not_here() {
    let f = Fixture::new().await;
    f.server.set_files(vec![File {
        file_path: "never-had-it.md".into(),
        content: None,
        deleted: true,
        ..Default::default()
    }]);

    let res = pull(&f.ctx).await.unwrap();
    assert!(res.removed.is_empty(), "removed {:?}", res.removed);
}

/// Nested paths from the server have to create their directories.
#[tokio::test]
async fn pull_creates_intermediate_directories() {
    let f = Fixture::new().await;
    f.server.set_files(vec![File {
        file_path: "topics/auth/tokens.md".into(),
        content: Some("# Tokens\n".into()),
        ..Default::default()
    }]);

    pull(&f.ctx).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(f.memory("topics/auth/tokens.md")).unwrap(),
        "# Tokens\n"
    );
}
