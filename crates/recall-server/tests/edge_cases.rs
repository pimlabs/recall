//! Edge cases for the server, chosen by asking what would actually destroy
//! or corrupt someone's memory rather than by covering lines.
//!
//! The first group is the important one: a merge is the only place this
//! server ever *replaces* content it already has, so every way a merge can
//! go wrong is a way to lose notes.

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use recall_server::merge::{Merger, Status};
use recall_server::{now, Config, Server, Store};
use recall_wire::{PushRequest, PushResponse, SyncResponse};
use tempfile::TempDir;
use tower::ServiceExt;

const TOKEN: &str = "edge-token";

struct Harness {
    server: Server,
    // Both are held only to keep the database file and the stand-in `claude`
    // script alive for the duration of a test; dropping either would delete
    // the temp dir out from under the server.
    _store: Arc<Store>,
    _dir: TempDir,
}

/// A server whose merge path is live, driven by a stand-in `claude` that
/// prints whatever `cli_output` says.
fn merging_harness(cli_output: &str) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("recall.db")).unwrap());
    let bin = fake_claude(dir.path(), cli_output);

    let server = Server::new(
        Config {
            token: TOKEN.to_string(),
            merge_enabled: true,
            claude_bin: bin,
            merge_timeout: Duration::from_secs(20),
            rate_limit_max: 10_000,
            ..Config::default()
        },
        store.clone(),
    );
    server.set_claude_status(Status {
        checked_at: now(),
        available: true,
        logged_in: true,
        error: String::new(),
    });
    Harness {
        server,
        _store: store,
        _dir: dir,
    }
}

fn plain_harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("recall.db")).unwrap());
    let server = Server::new(
        Config {
            token: TOKEN.to_string(),
            merge_enabled: false,
            rate_limit_max: 10_000,
            ..Config::default()
        },
        store.clone(),
    );
    Harness {
        server,
        _store: store,
        _dir: dir,
    }
}

impl Harness {
    async fn push(&self, body: &PushRequest) -> (StatusCode, Bytes) {
        let req = Request::builder()
            .method("POST")
            .uri("/sync")
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap();
        self.send(req).await
    }

    async fn send(&self, req: Request<Body>) -> (StatusCode, Bytes) {
        let resp = self.server.router().oneshot(req).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, body)
    }

    async fn write(&self, project: &str, path: &str, content: &str) -> PushResponse {
        let (status, body) = self
            .push(&PushRequest {
                project_key: project.into(),
                file_path: path.into(),
                content: Some(content.into()),
                source_env: "test".into(),
                deleted: false,
            })
            .await;
        assert_eq!(status, StatusCode::OK, "push rejected: {body:?}");
        serde_json::from_slice(&body).unwrap()
    }

    async fn stored(&self, project: &str, path: &str) -> Option<String> {
        let req = Request::builder()
            .method("GET")
            .uri(format!("/sync?project_key={}", urlencode(project)))
            .header("authorization", format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .unwrap();
        let (status, body) = self.send(req).await;
        assert_eq!(status, StatusCode::OK);
        let resp: SyncResponse = serde_json::from_slice(&body).unwrap();
        resp.files
            .into_iter()
            .find(|f| f.file_path == path)
            .and_then(|f| f.content)
    }
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn fake_claude(dir: &std::path::Path, out: &str) -> String {
    use std::io::Write;
    let path = dir.join("claude-edge");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(f, "#!/bin/sh").unwrap();
    writeln!(f, "cat > /dev/null").unwrap();
    writeln!(f, "printf '%s' '{out}'").unwrap();
    drop(f);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_str().unwrap().to_string()
}

// ---------------------------------------------------------------------------
// Merge: the only path that replaces content, so the only path that can lose it
// ---------------------------------------------------------------------------

/// The worst outcome this server can produce, and it was reachable: a merge
/// that comes back empty replaced both machines' notes with `""`, reported
/// `merged: true`, and the next pull would have written that empty file out
/// everywhere.
#[tokio::test]
async fn an_empty_merge_result_never_replaces_real_content() {
    let h = merging_harness(r#"{"is_error":false,"result":""}"#);

    h.write("acme/app", "MEMORY.md", "# Important\n- must not vanish\n")
        .await;
    let resp = h
        .write("acme/app", "MEMORY.md", "# Important\n- other fact\n")
        .await;

    assert!(
        !resp.merged,
        "an empty merge must not be reported as merged"
    );
    assert_eq!(
        h.stored("acme/app", "MEMORY.md").await.as_deref(),
        Some("# Important\n- other fact\n"),
        "must fall back to last-write-wins, not store the empty result"
    );
}

/// Whitespace-only is the same failure wearing a hat.
#[tokio::test]
async fn a_whitespace_only_merge_result_is_also_refused() {
    let h = merging_harness(r#"{"is_error":false,"result":"   \n\n  "}"#);

    h.write("acme/app", "MEMORY.md", "real content\n").await;
    let resp = h.write("acme/app", "MEMORY.md", "other content\n").await;

    assert!(!resp.merged);
    assert_eq!(
        h.stored("acme/app", "MEMORY.md").await.as_deref(),
        Some("other content\n")
    );
}

/// The guard must not fire when empty is the honest answer: two empty
/// versions merging to empty is correct, not a malfunction.
#[tokio::test]
async fn merging_two_empty_versions_may_legitimately_produce_empty() {
    let dir = tempfile::tempdir().unwrap();
    let merger = Merger::new(
        fake_claude(dir.path(), r#"{"is_error":false,"result":""}"#),
        Duration::from_secs(10),
    );
    assert_eq!(merger.merge("", "  ").await.unwrap(), "");
}

/// A CLI that prints something other than the expected envelope must not be
/// trusted to have produced a merge.
#[tokio::test]
async fn non_json_cli_output_falls_back_without_touching_content() {
    let h = merging_harness("not json at all");

    h.write("acme/app", "MEMORY.md", "first\n").await;
    let resp = h.write("acme/app", "MEMORY.md", "second\n").await;

    assert!(!resp.merged);
    assert_eq!(
        h.stored("acme/app", "MEMORY.md").await.as_deref(),
        Some("second\n")
    );
}

/// `is_error: true` means the CLI itself is telling us the result is not a
/// merge — believing the `result` field anyway would store an error message
/// as if it were the user's notes.
#[tokio::test]
async fn an_error_envelope_is_not_stored_as_content() {
    let h = merging_harness(r#"{"is_error":true,"result":"Credit balance too low"}"#);

    h.write("acme/app", "MEMORY.md", "first\n").await;
    let resp = h.write("acme/app", "MEMORY.md", "second\n").await;

    assert!(!resp.merged);
    let stored = h.stored("acme/app", "MEMORY.md").await.unwrap();
    assert_eq!(stored, "second\n");
    assert!(
        !stored.contains("Credit balance"),
        "the CLI's error text was stored as memory content"
    );
}

// ---------------------------------------------------------------------------
// Content fidelity
// ---------------------------------------------------------------------------

/// Memory files are prose written by a model; they will contain emoji, CJK,
/// combining marks and RTL text long before they contain anything exotic.
#[tokio::test]
async fn unicode_content_round_trips_exactly() {
    let h = plain_harness();
    let cases = [
        "# 日本語のメモ\n- 全角スペース　あり\n",
        "# Notes 🚀\n- emoji in a bullet ✅\n- family: 👨‍👩‍👧‍👦\n",
        "# Café\n- combining: e\u{0301} vs é\n",
        "# עברית\n- right to left\n",
        "# Zero width\u{200b}joiner\n",
        "trailing whitespace   \n\n\n",
    ];
    for (i, content) in cases.iter().enumerate() {
        let path = format!("note{i}.md");
        h.write("acme/app", &path, content).await;
        assert_eq!(
            h.stored("acme/app", &path).await.as_deref(),
            Some(*content),
            "case {i} altered in transit"
        );
    }
}

/// A NUL byte is valid UTF-8 and can legitimately appear in a text file.
/// SQLite's C API treats NUL as a terminator, so this checks the binding
/// isn't truncating on it.
#[tokio::test]
async fn a_nul_byte_in_content_is_not_a_truncation_point() {
    let h = plain_harness();
    let content = "before\u{0}after\n";
    h.write("acme/app", "nul.md", content).await;
    assert_eq!(
        h.stored("acme/app", "nul.md").await.as_deref(),
        Some(content),
        "content was truncated at the NUL byte"
    );
}

#[tokio::test]
async fn a_large_memory_file_round_trips() {
    let h = plain_harness();
    // Well under the 5 MiB body cap, but far past any buffer worth guessing.
    let content = "# Big\n".to_string() + &"- a line of notes\n".repeat(50_000);
    h.write("acme/app", "big.md", &content).await;
    assert_eq!(
        h.stored("acme/app", "big.md").await.as_deref(),
        Some(content.as_str())
    );
}

/// Past the cap the server must refuse cleanly rather than buffer it or die.
#[tokio::test]
async fn a_body_past_the_limit_is_rejected_not_absorbed() {
    let h = plain_harness();
    let huge = "x".repeat(6 << 20);
    let (status, _) = h
        .push(&PushRequest {
            project_key: "acme/app".into(),
            file_path: "huge.md".into(),
            content: Some(huge),
            source_env: "test".into(),
            deleted: false,
        })
        .await;
    assert!(
        status == StatusCode::PAYLOAD_TOO_LARGE || status == StatusCode::BAD_REQUEST,
        "expected a clean refusal, got {status}"
    );
    assert_eq!(
        h.stored("acme/app", "huge.md").await,
        None,
        "an over-limit push must not be partially stored"
    );
}

// ---------------------------------------------------------------------------
// Keys and paths
// ---------------------------------------------------------------------------

/// `project_key` is `owner/repo`, so it always contains a slash — and the
/// derivation lowercases but does not otherwise sanitize. These have to
/// survive the query string intact.
#[tokio::test]
async fn awkward_project_keys_round_trip() {
    let h = plain_harness();
    for key in [
        "acme/app",
        "acme/app-with-dashes",
        "acme/app.with.dots",
        "acme/app+plus",
        "acme/app&ampersand",
        "acme/app#hash",
        "acme/app?question",
        "acme/app with spaces",
        "acme/日本語",
        "local:-home-user-project",
    ] {
        h.write(key, "MEMORY.md", key).await;
        assert_eq!(
            h.stored(key, "MEMORY.md").await.as_deref(),
            Some(key),
            "project_key {key:?} did not round-trip"
        );
    }
}

/// Two keys that differ only after a character the query string has to
/// escape must not collide.
#[tokio::test]
async fn project_keys_that_differ_only_by_an_escaped_character_stay_separate() {
    let h = plain_harness();
    h.write("acme/app#one", "MEMORY.md", "first").await;
    h.write("acme/app#two", "MEMORY.md", "second").await;

    assert_eq!(
        h.stored("acme/app#one", "MEMORY.md").await.as_deref(),
        Some("first")
    );
    assert_eq!(
        h.stored("acme/app#two", "MEMORY.md").await.as_deref(),
        Some("second")
    );
}

#[tokio::test]
async fn deeply_nested_and_awkward_file_paths_round_trip() {
    let h = plain_harness();
    for path in [
        "a/b/c/d/e/f/g/h/i/j/deep.md",
        "topics/with space.md",
        "topics/émoji-🚀.md",
        "..config.md",
        "dots...md",
        &format!("{}.md", "n".repeat(200)),
    ] {
        h.write("acme/app", path, "content").await;
        assert_eq!(
            h.stored("acme/app", path).await.as_deref(),
            Some("content"),
            "file_path {path:?} did not round-trip"
        );
    }
}

// ---------------------------------------------------------------------------
// Tombstones
// ---------------------------------------------------------------------------

/// Reconciliation reports a delete for anything missing, which includes
/// files the server has never heard of — a fresh clone whose baseline is
/// ahead of the server, for instance.
#[tokio::test]
async fn tombstoning_a_file_the_server_never_had_is_accepted() {
    let h = plain_harness();
    let (status, _) = h
        .push(&PushRequest {
            project_key: "acme/app".into(),
            file_path: "never-existed.md".into(),
            content: None,
            source_env: "test".into(),
            deleted: true,
        })
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.stored("acme/app", "never-existed.md").await, None);
}

/// Reviving a tombstone must not merge against the content the delete was
/// expressing an intent to discard.
#[tokio::test]
async fn reviving_a_tombstone_does_not_merge_against_the_dead_content() {
    let h = merging_harness(r#"{"is_error":false,"result":"MERGED - should not appear"}"#);

    h.write("acme/app", "MEMORY.md", "old content\n").await;
    h.push(&PushRequest {
        project_key: "acme/app".into(),
        file_path: "MEMORY.md".into(),
        content: None,
        source_env: "test".into(),
        deleted: true,
    })
    .await;

    let resp = h.write("acme/app", "MEMORY.md", "brand new\n").await;
    assert!(!resp.merged, "a revived tombstone must not trigger a merge");
    assert_eq!(
        h.stored("acme/app", "MEMORY.md").await.as_deref(),
        Some("brand new\n")
    );
}

/// Re-pushing content the server already has is not a conflict, so it must
/// not spend a merge call on it.
#[tokio::test]
async fn an_identical_re_push_does_not_merge() {
    let h = merging_harness(r#"{"is_error":false,"result":"MERGED - should not appear"}"#);
    h.write("acme/app", "MEMORY.md", "same\n").await;
    let resp = h.write("acme/app", "MEMORY.md", "same\n").await;
    assert!(!resp.merged);
    assert_eq!(
        h.stored("acme/app", "MEMORY.md").await.as_deref(),
        Some("same\n")
    );
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

#[tokio::test]
async fn authorization_header_variants_are_rejected_precisely() {
    let h = plain_harness();
    let uri = "/sync?project_key=acme%2Fapp";

    for (header, why) in [
        ("", "empty header"),
        ("Bearer", "scheme with no token"),
        ("Bearer ", "scheme with empty token"),
        ("bearer edge-token", "lowercase scheme"),
        ("Basic edge-token", "wrong scheme"),
        ("Bearer edge-token-extra", "token with a suffix"),
        ("Bearer edge-toke", "token that is a prefix of the real one"),
        ("Bearer  edge-token", "extra space before the token"),
        ("Bearer edge-token ", "trailing space after the token"),
    ] {
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .header("authorization", header)
            .body(Body::empty())
            .unwrap();
        let (status, _) = h.send(req).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{why} ({header:?}) should not authenticate"
        );
    }

    // And the exact value still works, so the checks above aren't passing
    // for the wrong reason.
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(h.send(req).await.0, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

/// The store is behind one mutex; this asserts that holds under real
/// contention rather than by inspection, and that no write is lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_pushes_to_one_project_all_land() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("recall.db")).unwrap());
    let server = Arc::new(Server::new(
        Config {
            token: TOKEN.to_string(),
            merge_enabled: false,
            rate_limit_max: 10_000,
            ..Config::default()
        },
        store.clone(),
    ));

    let mut tasks = Vec::new();
    for i in 0..40 {
        let server = server.clone();
        tasks.push(tokio::spawn(async move {
            let body = PushRequest {
                project_key: "acme/app".into(),
                file_path: format!("file{i}.md"),
                content: Some(format!("content {i}")),
                source_env: "test".into(),
                deleted: false,
            };
            let req = Request::builder()
                .method("POST")
                .uri("/sync")
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap();
            server.router().oneshot(req).await.unwrap().status()
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), StatusCode::OK);
    }

    let files = store.list("acme/app").unwrap();
    assert_eq!(files.len(), 40, "a concurrent push was lost");
}

/// Racing writers to the *same* row must leave one of the two values, not a
/// blend or a corrupted row.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_pushes_to_one_file_leave_a_coherent_value() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("recall.db")).unwrap());
    let server = Arc::new(Server::new(
        Config {
            token: TOKEN.to_string(),
            merge_enabled: false,
            rate_limit_max: 10_000,
            ..Config::default()
        },
        store.clone(),
    ));

    let mut tasks = Vec::new();
    for i in 0..20 {
        let server = server.clone();
        tasks.push(tokio::spawn(async move {
            let body = PushRequest {
                project_key: "acme/app".into(),
                file_path: "MEMORY.md".into(),
                content: Some(format!("writer {i}")),
                source_env: "test".into(),
                deleted: false,
            };
            let req = Request::builder()
                .method("POST")
                .uri("/sync")
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap();
            server.router().oneshot(req).await.unwrap().status()
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), StatusCode::OK);
    }

    let files = store.list("acme/app").unwrap();
    assert_eq!(files.len(), 1);
    let content = files[0].content.clone().unwrap();
    assert!(
        (0..20).any(|i| content == format!("writer {i}")),
        "row is neither writer's value: {content:?}"
    );
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/// A backup destination that cannot be created must not take the server
/// down — backups are best-effort by design.
///
/// The unwritable directory is built by putting a regular file where a
/// parent directory needs to be, rather than by chmod: CI and this project's
/// own container run tests as root, and root walks straight through mode
/// bits, so a `0o500` directory would quietly not be a negative test at all.
#[test]
fn a_backup_into_an_uncreatable_directory_fails_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("recall.db")).unwrap();

    let blocker = dir.path().join("not-a-directory");
    std::fs::write(&blocker, b"i am a file").unwrap();
    let unusable = blocker.join("backups");

    let result = store.backup(&unusable, 7);
    assert!(result.is_err(), "expected an error, not a panic or success");

    // And the store still works afterwards.
    store
        .upsert("acme/app", "MEMORY.md", "still fine", "test", &now())
        .unwrap();
    assert_eq!(store.list("acme/app").unwrap().len(), 1);
}

/// Reopening the same file must find the same rows — the property the whole
/// migration plan rests on.
#[test]
fn data_survives_closing_and_reopening_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recall.db");

    {
        let store = Store::open(&path).unwrap();
        store
            .upsert("acme/app", "MEMORY.md", "durable\n", "laptop", &now())
            .unwrap();
        store
            .tombstone("acme/app", "gone.md", "laptop", &now())
            .unwrap();
    }

    let reopened = Store::open(&path).unwrap();
    let files = reopened.list("acme/app").unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(
        files
            .iter()
            .find(|f| f.file_path == "MEMORY.md")
            .unwrap()
            .content
            .as_deref(),
        Some("durable\n")
    );
    assert!(
        files
            .iter()
            .find(|f| f.file_path == "gone.md")
            .unwrap()
            .deleted
    );
}
