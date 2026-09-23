//! The `recall-server` binary itself: what it prints, and what it refuses.
//!
//! Everything it serves is tested through the router in `server.rs`. These
//! are the few things only the process can show: its arguments, its exit
//! codes, and that it will not start without a token.

use std::process::{Command, Output};

fn run(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_recall-server"));
    cmd.args(args).env_clear();
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("recall-server runs")
}

/// The server is the one program allowed to refuse to start: one reachable
/// from the internet with no token is not a degraded mode.
#[test]
fn refuses_to_start_without_a_token() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("x.db");
    let out = run(&[], &[("RECALL_DB_PATH", &db.to_string_lossy())]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("RECALL_TOKEN"), "stderr: {stderr}");
}

/// `version`, `--version` and `-V` agree, and start with the binary's name
/// and version, the shape `recall version` has.
#[test]
fn every_way_of_asking_for_the_version_says_the_same() {
    let answers: Vec<String> = ["version", "--version", "-V"]
        .iter()
        .map(|a| {
            let out = run(&[a], &[]);
            assert_eq!(out.status.code(), Some(0));
            String::from_utf8_lossy(&out.stdout).to_string()
        })
        .collect();
    let want = format!("recall-server {} (", env!("CARGO_PKG_VERSION"));
    assert!(answers[0].starts_with(&want), "got {:?}", answers[0]);
    assert_eq!(answers[0], answers[1]);
    assert_eq!(answers[0], answers[2]);
}

/// Anything else is a usage error, exit 2, and it does not start serving.
#[test]
fn an_unknown_argument_is_a_usage_error() {
    let out = run(&["serve"], &[("RECALL_TOKEN", "t")]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr: {stderr}");
    assert!(stderr.contains("Usage: recall-server"), "stderr: {stderr}");
}

/// The way back in for an owner who lost every passkey: a command run where
/// the database is, never a route the token can reach.
#[test]
fn reset_passkeys_empties_the_passkeys_and_needs_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("r.db");
    let db_str = db.to_string_lossy().to_string();
    let out = run(&["reset-passkeys"], &[("RECALL_DB_PATH", &db_str)]);
    assert_eq!(out.status.code(), Some(1), "no database there yet");

    let store = recall_server::Store::open(&db).unwrap();
    store
        .add_admin_credential(
            &recall_server::store::NewAdminCredential {
                id: "cred",
                user_handle: "u",
                name: "phone",
                passkey: "{}",
                sign_count: 0,
                created_at: "2026-09-23T10:00:00.000Z",
            },
            true,
        )
        .unwrap();
    drop(store);
    let out = run(&["reset-passkeys"], &[("RECALL_DB_PATH", &db_str)]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert!(stdout.contains("Removed 1 passkey"), "{stdout}");
    let store = recall_server::Store::open(&db).unwrap();
    assert!(!store.has_admin_credentials().unwrap());
}
