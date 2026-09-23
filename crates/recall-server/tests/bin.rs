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

/// Half a TLS pair, either kind, refuses to start rather than falling back
/// to plain HTTP: silently ignoring `RECALL_TLS_KEY` without a
/// `RECALL_TLS_CERT` (or vice versa) would run without any TLS at all,
/// which is not what setting one of the two variables meant.
#[test]
fn half_a_tls_pair_refuses_to_start() {
    for (env, want) in [
        (
            vec![("RECALL_TOKEN", "t"), ("RECALL_TLS_CERT", "/x/cert.pem")],
            "RECALL_TLS_CERT and RECALL_TLS_KEY",
        ),
        (
            vec![
                ("RECALL_TOKEN", "t"),
                ("RECALL_TLS_ACME_EMAIL", "me@example.com"),
            ],
            "RECALL_TLS_ACME_DOMAINS and RECALL_TLS_ACME_EMAIL",
        ),
    ] {
        let out = run(&[], &env);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
        assert!(stderr.contains(want), "stderr: {stderr}");
    }
}

/// Configuring both TLS modes at once refuses to start: each names its own
/// certificate source, and nothing picks a winner between them.
#[test]
fn both_tls_modes_at_once_refuses_to_start() {
    let out = run(
        &[],
        &[
            ("RECALL_TOKEN", "t"),
            ("RECALL_TLS_CERT", "/x/cert.pem"),
            ("RECALL_TLS_KEY", "/x/key.pem"),
            ("RECALL_TLS_ACME_DOMAINS", "example.com"),
            ("RECALL_TLS_ACME_EMAIL", "me@example.com"),
        ],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains("two different TLS modes"),
        "stderr: {stderr}"
    );
}

/// The mandatory security rule, proved against the real binary rather than
/// just `Config::from_lookup`: with direct TLS there is no ingress to set
/// `RECALL_TRUSTED_IP_HEADER`, so a deployment that sets it anyway is
/// refused rather than quietly trusting a header any direct client could
/// forge to buy itself unlimited token guesses. `scripts/trusted-ip-check.sh`
/// proves the same rule against a live socket.
#[test]
fn trusted_ip_header_with_tls_refuses_to_start() {
    for tls_env in [
        vec![
            ("RECALL_TLS_CERT", "/x/cert.pem"),
            ("RECALL_TLS_KEY", "/x/key.pem"),
        ],
        vec![
            ("RECALL_TLS_ACME_DOMAINS", "example.com"),
            ("RECALL_TLS_ACME_EMAIL", "me@example.com"),
        ],
    ] {
        let mut env = vec![
            ("RECALL_TOKEN", "t"),
            ("RECALL_TRUSTED_IP_HEADER", "x-real-ip"),
        ];
        env.extend(tls_env);
        let out = run(&[], &env);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
        assert!(
            stderr.contains("RECALL_TRUSTED_IP_HEADER"),
            "stderr: {stderr}"
        );
    }
}

/// `docker-compose.direct.yml` sets `RECALL_TLS_REQUIRED=true` and publishes
/// its port to the internet, so if its certificate variables ever arrive
/// empty the process must exit rather than serve the bearer token over
/// plain HTTP. Proved against the real binary: the refusal happens before
/// anything binds a port.
#[test]
fn tls_required_without_tls_refuses_to_start() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("x.db");
    let db = db.to_string_lossy();
    let out = run(
        &[],
        &[
            ("RECALL_TOKEN", "t"),
            ("RECALL_DB_PATH", &db),
            ("RECALL_TLS_REQUIRED", "true"),
            ("RECALL_TLS_ACME_DOMAINS", ""),
            ("RECALL_TLS_ACME_EMAIL", ""),
            ("RECALL_TRUSTED_IP_HEADER", ""),
        ],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("RECALL_TLS_REQUIRED"), "stderr: {stderr}");
    assert!(!stderr.contains("listening"), "stderr: {stderr}");
}
