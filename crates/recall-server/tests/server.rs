//! Ported from internal/server/server_test.go — the behaviors here are the
//! frozen ones: auth posture, tombstone semantics, project isolation, the
//! merge fallback, and rate limiting.

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use recall_server::merge::Status;
use recall_server::{now, Config, Server, Store};
use recall_wire::{AdminStats, Health, PushRequest, PushResponse, SyncResponse};
use tempfile::TempDir;
use tower::ServiceExt;

const TEST_TOKEN: &str = "test-token";

struct Harness {
    server: Server,
    store: Arc<Store>,
    dir: TempDir,
}

fn harness(tweak: impl FnOnce(&mut Config)) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("recall.db")).unwrap());
    let mut cfg = Config {
        token: TEST_TOKEN.to_string(),
        git_commit: "testcommit".to_string(),
        rate_limit_window: Duration::from_secs(60),
        rate_limit_max: 1000,
        merge_enabled: false,
        claude_bin: "definitely-not-a-real-binary".to_string(),
        ..Config::default()
    };
    tweak(&mut cfg);
    let server = Server::new(cfg, store.clone());
    Harness { server, store, dir }
}

impl Harness {
    async fn send(&self, req: Request<Body>) -> (StatusCode, HeaderMap, Bytes) {
        let resp = self.server.router().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, headers, body)
    }

    async fn get(&self, token: Option<&str>, path: &str) -> (StatusCode, HeaderMap, Bytes) {
        let mut req = Request::builder().method("GET").uri(path);
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        self.send(req.body(Body::empty()).unwrap()).await
    }

    async fn push(&self, token: &str, body: &PushRequest) -> (StatusCode, Bytes) {
        let req = Request::builder()
            .method("POST")
            .uri("/sync")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap();
        let (status, _, bytes) = self.send(req).await;
        (status, bytes)
    }

    async fn ok_push(&self, project_key: &str, file_path: &str, content: &str, source_env: &str) {
        let (status, _) = self
            .push(
                TEST_TOKEN,
                &PushRequest {
                    project_key: project_key.into(),
                    file_path: file_path.into(),
                    content: Some(content.into()),
                    source_env: source_env.into(),
                    deleted: false,
                    base_sha256: None,
                },
            )
            .await;
        assert_eq!(status, StatusCode::OK, "push failed");
    }

    async fn ok_delete(&self, project_key: &str, file_path: &str, source_env: &str) {
        let (status, _) = self
            .push(
                TEST_TOKEN,
                &PushRequest {
                    project_key: project_key.into(),
                    file_path: file_path.into(),
                    source_env: source_env.into(),
                    deleted: true,
                    ..Default::default()
                },
            )
            .await;
        assert_eq!(status, StatusCode::OK, "delete failed");
    }

    async fn pull(&self, project_key: &str) -> (SyncResponse, Bytes) {
        let (status, _, body) = self
            .get(
                Some(TEST_TOKEN),
                &format!("/sync?project_key={project_key}"),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        (serde_json::from_slice(&body).unwrap(), body)
    }
}

#[tokio::test]
async fn auth_rejects_missing_and_wrong_tokens() {
    let h = harness(|_| {});

    let (status, _, _) = h.get(None, "/sync?project_key=acme/app").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "no token");

    let (status, _, _) = h
        .get(Some("wrong-token"), "/sync?project_key=acme/app")
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "wrong token");

    let (status, _, _) = h.get(Some(TEST_TOKEN), "/sync?project_key=acme/app").await;
    assert_eq!(status, StatusCode::OK, "valid token");
}

/// `/health` has to stay pollable by uptime tooling that holds no secret.
#[tokio::test]
async fn health_needs_no_token() {
    let h = harness(|_| {});
    let (status, _, body) = h.get(None, "/health").await;
    assert_eq!(status, StatusCode::OK);
    let health: Health = serde_json::from_slice(&body).unwrap();
    assert_eq!(health.status, "ok");
    assert_eq!(health.git_commit, "testcommit");
    assert!(!health.started_at.is_empty());
}

#[tokio::test]
async fn push_then_pull_round_trip() {
    let h = harness(|_| {});
    let content = "# Memory\n- a fact\n";
    h.ok_push("acme/app", "MEMORY.md", content, "laptop").await;

    let (out, _) = h.pull("acme/app").await;
    assert_eq!(out.project_key, "acme/app");
    assert_eq!(out.files.len(), 1);
    assert_eq!(out.files[0].content.as_deref(), Some(content));
    assert_eq!(out.files[0].source_env, "laptop");
    assert!(!out.files[0].updated_at.is_empty());
}

#[tokio::test]
async fn rejects_traversal_and_absolute_paths() {
    let h = harness(|_| {});
    for bad in ["../escape.md", "/etc/passwd", "a/../../b.md"] {
        let (status, _) = h
            .push(
                TEST_TOKEN,
                &PushRequest {
                    project_key: "acme/app".into(),
                    file_path: bad.into(),
                    content: Some("x".into()),
                    ..Default::default()
                },
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "file_path {bad:?}");
    }
}

/// A tombstone keeps its content in the database for recovery, but must not
/// hand it back — otherwise a pull would resurrect a deleted file.
#[tokio::test]
async fn tombstone_withholds_content_but_the_database_keeps_it() {
    let h = harness(|_| {});
    let secret = "# Secret\n- do not resurrect\n";
    h.ok_push("acme/app", "gone.md", secret, "laptop").await;
    h.ok_delete("acme/app", "gone.md", "laptop").await;

    let (out, raw) = h.pull("acme/app").await;
    assert_eq!(out.files.len(), 1);
    assert!(out.files[0].deleted, "file not reported as deleted");
    assert_eq!(out.files[0].content, None, "tombstoned content was served");
    assert!(
        !String::from_utf8_lossy(&raw).contains("do not resurrect"),
        "tombstoned content leaked into the response body"
    );

    // Still recoverable at the database level, which is the point of a
    // tombstone rather than a delete.
    let stored = h.store.get("acme/app", "gone.md").unwrap().unwrap();
    assert_eq!(stored.content, secret, "content was not preserved");
    assert!(stored.deleted);
}

#[tokio::test]
async fn projects_are_isolated() {
    let h = harness(|_| {});
    h.ok_push("acme/website", "MEMORY.md", "website", "laptop")
        .await;
    h.ok_push("acme/backend", "MEMORY.md", "backend", "laptop")
        .await;
    h.ok_delete("acme/website", "MEMORY.md", "laptop").await;

    let (out, _) = h.pull("acme/backend").await;
    assert_eq!(out.files.len(), 1);
    assert!(
        !out.files[0].deleted,
        "deleting in one project affected another"
    );
    assert_eq!(out.files[0].content.as_deref(), Some("backend"));

    let (gone, _) = h.pull("acme/website").await;
    assert!(gone.files[0].deleted);
}

/// With merge enabled and the CLI reporting logged in, a broken binary must
/// still leave the push accepted: every merge failure degrades to
/// last-write-wins and still returns 200.
#[tokio::test]
async fn merge_failure_falls_back_to_last_write_wins() {
    let h = harness(|c| c.merge_enabled = true);
    h.server.set_claude_status(Status {
        checked_at: now(),
        available: true,
        logged_in: true,
        error: String::new(),
    });

    h.ok_push("acme/app", "MEMORY.md", "version A", "laptop")
        .await;
    let (status, body) = h
        .push(
            TEST_TOKEN,
            &PushRequest {
                project_key: "acme/app".into(),
                file_path: "MEMORY.md".into(),
                content: Some("version B".into()),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK, "conflicting push rejected");
    let pr: PushResponse = serde_json::from_slice(&body).unwrap();
    assert!(!pr.merged, "reported a merge without a working claude CLI");

    let (out, _) = h.pull("acme/app").await;
    assert_eq!(
        out.files[0].content.as_deref(),
        Some("version B"),
        "want last-write-wins"
    );

    // The failure is visible on /health rather than swallowed.
    let (_, _, health) = h.get(None, "/health").await;
    let health: Health = serde_json::from_slice(&health).unwrap();
    let err = health.merge.last_merge_error.expect("last_merge_error");
    assert!(!err.message.is_empty() && !err.at.is_empty());
}

/// No merge is even attempted while the CLI is known logged out: every
/// attempt would burn a subprocess and a timeout before failing to the same
/// place.
#[tokio::test]
async fn merge_is_not_attempted_when_the_cli_is_logged_out() {
    let h = harness(|c| c.merge_enabled = true);
    h.ok_push("acme/app", "MEMORY.md", "version A", "laptop")
        .await;
    h.ok_push("acme/app", "MEMORY.md", "version B", "laptop")
        .await;

    let (_, _, body) = h.get(None, "/health").await;
    let health: Health = serde_json::from_slice(&body).unwrap();
    assert!(
        health.merge.last_merge_error.is_none(),
        "a merge was attempted with no usable CLI"
    );
    let (out, _) = h.pull("acme/app").await;
    assert_eq!(out.files[0].content.as_deref(), Some("version B"));
}

/// The whole merge path, with a stand-in CLI: a genuine conflict is
/// reconciled and the merged text is what gets stored.
#[tokio::test]
async fn a_real_conflict_is_merged_end_to_end() {
    let h = harness(|_| {});
    let bin = fake_claude(h.dir.path(), r#"{"is_error":false,"result":"A and B"}"#);
    let h = Harness {
        server: Server::new(
            Config {
                token: TEST_TOKEN.to_string(),
                merge_enabled: true,
                claude_bin: bin,
                merge_timeout: Duration::from_secs(20),
                rate_limit_max: 1000,
                ..Config::default()
            },
            h.store.clone(),
        ),
        ..h
    };
    h.server.set_claude_status(Status {
        checked_at: now(),
        available: true,
        logged_in: true,
        error: String::new(),
    });

    h.ok_push("acme/app", "MEMORY.md", "A", "laptop").await;
    let (status, body) = h
        .push(
            TEST_TOKEN,
            &PushRequest {
                project_key: "acme/app".into(),
                file_path: "MEMORY.md".into(),
                content: Some("B".into()),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let pr: PushResponse = serde_json::from_slice(&body).unwrap();
    assert!(pr.merged, "push was not reported as merged");

    let (out, _) = h.pull("acme/app").await;
    assert_eq!(out.files[0].content.as_deref(), Some("A and B"));

    // A re-push of the identical content is not a conflict, so the CLI must
    // not be consulted again.
    let (_, body) = h
        .push(
            TEST_TOKEN,
            &PushRequest {
                project_key: "acme/app".into(),
                file_path: "MEMORY.md".into(),
                content: Some("A and B".into()),
                ..Default::default()
            },
        )
        .await;
    let pr: PushResponse = serde_json::from_slice(&body).unwrap();
    assert!(!pr.merged, "an unchanged re-push must skip the merge");
}

/// The distinction the base exists for. The next edit replaces what it
/// started from — deletions included, which a merge cannot express, since it
/// keeps every distinct fact from both versions. Only an edit made against
/// something other than what is stored is a concurrent one, and only that is
/// merged. A client that sends no base keeps the old behaviour.
#[tokio::test]
async fn only_a_concurrent_edit_is_merged() {
    let h = harness(|_| {});
    let bin = fake_claude(h.dir.path(), r#"{"is_error":false,"result":"MERGED"}"#);
    let h = Harness {
        server: Server::new(
            Config {
                token: TEST_TOKEN.to_string(),
                merge_enabled: true,
                claude_bin: bin,
                merge_timeout: Duration::from_secs(20),
                rate_limit_max: 1000,
                ..Config::default()
            },
            h.store.clone(),
        ),
        ..h
    };
    h.server.set_claude_status(Status {
        checked_at: now(),
        available: true,
        logged_in: true,
        error: String::new(),
    });

    let push = |content: &str, base: Option<String>| {
        let req = PushRequest {
            project_key: "acme/app".into(),
            file_path: "MEMORY.md".into(),
            content: Some(content.into()),
            base_sha256: base,
            ..Default::default()
        };
        let h = &h;
        async move {
            let (status, body) = h.push(TEST_TOKEN, &req).await;
            assert_eq!(status, StatusCode::OK);
            serde_json::from_slice::<PushResponse>(&body)
                .unwrap()
                .merged
        }
    };
    let stored = || async { h.pull("acme/app").await.0.files[0].content.clone().unwrap() };

    let a = "keep
drop this line
";
    assert!(!push(a, None).await, "a new file has nothing to merge with");

    // The next edit, made from what is stored: it replaces it outright, and
    // the deleted line stays deleted.
    let b = "keep
";
    assert!(
        !push(b, Some(recall_wire::content_sha256(a))).await,
        "the next edit was merged — the deleted line would come back"
    );
    assert_eq!(stored().await, b);

    // An edit from a version that is no longer stored: someone else wrote in
    // between, and that is exactly what the merge is for.
    assert!(
        push(
            "keep
other
",
            Some(recall_wire::content_sha256(a))
        )
        .await,
        "a concurrent edit was not merged"
    );
    assert_eq!(stored().await, "MERGED");

    // No base: an older client, treated as it always was.
    assert!(
        push(
            "anything
",
            None
        )
        .await,
        "a push with no base must still merge"
    );

    // Hex is hex; a client that uppercases it is still naming the same base.
    assert!(
        !push(
            "final
",
            Some(recall_wire::content_sha256("MERGED").to_uppercase())
        )
        .await
    );
    assert_eq!(stored().await, "final\n");
}

#[tokio::test]
async fn rate_limit_triggers_and_health_is_exempt() {
    let h = harness(|c| c.rate_limit_max = 3);

    let mut last = StatusCode::OK;
    for _ in 0..5 {
        let (status, _, _) = h.get(Some(TEST_TOKEN), "/sync?project_key=acme/app").await;
        last = status;
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);

    // /health is deliberately exempt so uptime polling can't be locked out.
    let (status, _, _) = h.get(None, "/health").await;
    assert_eq!(status, StatusCode::OK, "/health while rate limited");
}

/// Invalid tokens must be counted too, or a flood of them escapes the
/// limiter by never reaching the auth check.
#[tokio::test]
async fn rate_limit_counts_unauthorized_requests() {
    let h = harness(|c| c.rate_limit_max = 3);

    let mut last = StatusCode::OK;
    let mut retry_after = None;
    for _ in 0..5 {
        let (status, headers, _) = h.get(Some("bad-token"), "/sync?project_key=acme/app").await;
        last = status;
        retry_after = headers
            .get("retry-after")
            .map(|v| v.to_str().unwrap().to_string());
    }
    assert_eq!(
        last,
        StatusCode::TOO_MANY_REQUESTS,
        "bad tokens must be rate limited too"
    );
    assert_eq!(retry_after.as_deref(), Some("60"));
}

#[tokio::test]
async fn admin_page_serves_without_leaking_data() {
    let h = harness(|_| {});
    h.ok_push("acme/app", "MEMORY.md", "sensitive note", "laptop")
        .await;

    let (status, headers, body) = h.get(None, "/admin").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !String::from_utf8_lossy(&body).contains("sensitive note"),
        "admin page embedded memory content server-side"
    );
    assert!(
        headers.contains_key("content-security-policy"),
        "admin page served without a CSP"
    );
    assert_eq!(headers.get("x-content-type-options").unwrap(), "nosniff");

    let (status, _, _) = h.get(None, "/admin/stats").await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "/admin/stats without a token"
    );
}

#[tokio::test]
async fn admin_stats_totals() {
    let h = harness(|_| {});
    h.ok_push("acme/app", "MEMORY.md", "a", "laptop").await;
    h.ok_push("acme/app", "gone.md", "b", "cloud").await;
    h.ok_delete("acme/app", "gone.md", "cloud").await;

    let (status, _, body) = h.get(Some(TEST_TOKEN), "/admin/stats").await;
    assert_eq!(status, StatusCode::OK);
    let stats: AdminStats = serde_json::from_slice(&body).unwrap();
    assert_eq!(stats.totals.project_count, 1);
    assert_eq!(stats.totals.file_count, 1);
    assert_eq!(stats.totals.deleted_count, 1);
    assert_eq!(stats.projects.len(), 1);
    assert_eq!(stats.projects[0].sources, vec!["cloud", "laptop"]);
    assert_eq!(stats.git_commit, "testcommit");
}

#[tokio::test]
async fn backup_produces_a_restorable_snapshot() {
    let h = harness(|_| {});
    h.ok_push("acme/app", "MEMORY.md", "backed up", "laptop")
        .await;

    let backups = tempfile::tempdir().unwrap();
    let dest = h.store.backup(backups.path(), 7).unwrap();

    // The snapshot must be a usable database, not just bytes on disk.
    let restored = Store::open(&dest).unwrap();
    let files = restored.list("acme/app").unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].content.as_deref(), Some("backed up"));
}

#[tokio::test]
async fn backup_prunes_old_snapshots() {
    let h = harness(|_| {});
    let backups = tempfile::tempdir().unwrap();
    for _ in 0..5 {
        h.store.backup(backups.path(), 2).unwrap();
        // Distinct millisecond stamps: same-millisecond names would collide
        // and VACUUM INTO refuses to overwrite.
        std::thread::sleep(Duration::from_millis(2));
    }
    let kept: Vec<_> = std::fs::read_dir(backups.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("recall-") && n.ends_with(".db"))
        .collect();
    assert_eq!(kept.len(), 2, "kept {kept:?}");
}

#[tokio::test]
async fn unknown_routes_and_methods_are_404_json() {
    let h = harness(|_| {});

    let (status, _, body) = h.get(None, "/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["error"],
        "not found"
    );

    let req = Request::builder()
        .method("PUT")
        .uri("/sync")
        .header("authorization", format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let (status, _, _) = h.send(req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn malformed_pushes_are_400() {
    let h = harness(|_| {});

    let bad = |body: &'static str| {
        Request::builder()
            .method("POST")
            .uri("/sync")
            .header("authorization", format!("Bearer {TEST_TOKEN}"))
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    };
    for body in ["not json", "{}", r#"{"project_key":"acme/app"}"#] {
        let (status, _, _) = h.send(bad(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "body {body:?}");
    }

    let (status, _, _) = h.get(Some(TEST_TOKEN), "/sync").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "pull without project_key");
}

#[tokio::test]
async fn serve_shuts_down_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("recall.db")).unwrap());
    let server = Server::new(
        Config {
            token: TEST_TOKEN.to_string(),
            merge_enabled: false,
            ..Config::default()
        },
        store,
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serving = tokio::spawn(async move {
        server
            .serve_with_shutdown(listener, async {
                tokio::time::sleep(Duration::from_millis(100)).await;
            })
            .await
    });

    // Really bound and accepting before the shutdown fires.
    drop(tokio::net::TcpStream::connect(addr).await.unwrap());

    let done = tokio::time::timeout(Duration::from_secs(5), serving)
        .await
        .expect("server did not shut down within 5s")
        .expect("serve task panicked");
    assert!(done.is_ok(), "shutdown returned {done:?}");
}

/// A stand-in `claude` that drains stdin, ignores its arguments and prints
/// `out`.
fn fake_claude(dir: &std::path::Path, out: &str) -> String {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let path = dir.join("claude");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(f, "#!/bin/sh").unwrap();
    writeln!(f, "cat > /dev/null").unwrap();
    writeln!(f, "printf '%s' '{out}'").unwrap();
    drop(f);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_str().unwrap().to_string()
}

// ---------------------------------------------------------------------------
// discovery and the protocol header
// ---------------------------------------------------------------------------

/// A client needs to know what it is talking to before it knows whether it
/// can authenticate, so discovery asks for no token.
#[tokio::test]
async fn discovery_needs_no_token_and_describes_the_server() {
    let h = harness(|_| {});
    let (status, _, body) = h.get(None, recall_wire::DISCOVERY_PATH).await;
    assert_eq!(status, StatusCode::OK);
    let doc: recall_wire::Discovery = serde_json::from_slice(&body).unwrap();
    assert!(doc.speaks(recall_wire::PROTOCOL));
    assert_eq!(doc.protocol.current, recall_wire::PROTOCOL);
    // The deploy's commit is the build's revision, and a build that is not
    // a release says so and reports a -dev version.
    assert_eq!(doc.server.build.revision.as_deref(), Some("testcommit"));
    assert_eq!(doc.server.build.channel, "dev");
    assert!(
        doc.server.version.contains("-dev"),
        "{}",
        doc.server.version
    );
    assert!(recall_wire::discovery::Version::parse(&doc.min_client).is_some());
    // Nothing is removed within a protocol version: bearer stays, where it
    // was, and device signatures are added after it.
    assert_eq!(
        doc.auth.methods,
        vec!["bearer".to_string(), "device-sig-v1".to_string()]
    );
    assert!(doc.can("merge_base") && doc.can("scopes") && doc.can("limits"));
    let devices = doc.devices().expect("the devices capability");
    assert_eq!(devices.enroll_path, recall_wire::devices::ENROLL_PATH);
    assert_eq!(
        (
            devices.code_ttl_seconds,
            devices.poll_interval_seconds,
            devices.signature_window_seconds
        ),
        (900, 5, 60)
    );
}

/// A client that names a protocol this server does not speak is told so,
/// before the token is even looked at: an upgrade is the fix, not a login.
#[tokio::test]
async fn an_unknown_protocol_is_refused_with_what_the_server_speaks() {
    let h = harness(|_| {});
    let req = Request::builder()
        .method("GET")
        .uri("/sync?project_key=acme/app")
        .header("authorization", format!("Bearer {TEST_TOKEN}"))
        .header(recall_wire::PROTOCOL_HEADER, "99")
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = h.send(req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("protocol 1") && text.contains("99"), "{text}");
}

/// Every client before the header existed sends none, and the one this
/// build ships sends 1. Both are served.
#[tokio::test]
async fn protocol_1_and_no_header_are_both_served() {
    let h = harness(|_| {});
    for header in [None, Some("1")] {
        let mut req = Request::builder()
            .method("GET")
            .uri("/sync?project_key=acme/app")
            .header("authorization", format!("Bearer {TEST_TOKEN}"));
        if let Some(v) = header {
            req = req.header(recall_wire::PROTOCOL_HEADER, v);
        }
        let (status, _, _) = h.send(req.body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK, "header {header:?}");
    }
}

/// Every push a released client has sent is still accepted, byte for byte.
/// The fixtures are the ones `recall-wire`'s golden tests read; this is the
/// same promise from the side that has to honour it.
#[tokio::test]
async fn every_push_a_released_client_sent_is_still_accepted() {
    let root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../recall-wire/fixtures/wire");
    let mut sent = 0;
    for version in std::fs::read_dir(&root).unwrap() {
        let version = version.unwrap().path();
        for name in ["push_request.json", "push_request_delete.json"] {
            let path = version.join(name);
            let Ok(body) = std::fs::read(&path) else {
                continue;
            };
            let h = harness(|_| {});
            let req = Request::builder()
                .method("POST")
                .uri("/sync")
                .header("authorization", format!("Bearer {TEST_TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap();
            let (status, _, resp) = h.send(req).await;
            assert_eq!(
                status,
                StatusCode::OK,
                "{}: {}",
                path.display(),
                String::from_utf8_lossy(&resp)
            );
            sent += 1;
        }
    }
    assert!(sent >= 4, "found only {sent} request fixtures");
}
