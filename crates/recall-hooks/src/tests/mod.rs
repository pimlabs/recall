//! Tests that span more than one module.
//!
//! Push, pull and the baseline are one behaviour seen from three angles — a
//! pull writes the baseline a later push reconciles against — so their tests
//! share a fixture and live together rather than being split across the
//! files they happen to exercise. Tests that belong to a single module (path
//! containment, payload parsing, the settings merge) stay next to it.

mod behaviour;
mod edge_cases;
mod global_scope;

use std::fs;
use std::path::{Path, PathBuf};

use crate::client::Client;
use crate::context::Context;
use crate::state;
use crate::testserver::FakeServer;

/// A memory directory, a baseline, and a real server — the three things
/// every test here needs.
pub(crate) struct Fixture {
    pub(crate) dir: tempfile::TempDir,
    pub(crate) server: FakeServer,
    pub(crate) ctx: Context,
}

impl Fixture {
    pub(crate) async fn new() -> Self {
        Self::with_global(None).await
    }

    /// A fixture whose memory directory also carries a global scope.
    pub(crate) async fn with_global_scope() -> Self {
        Self::with_global(Some("global:eko".to_string())).await
    }

    async fn with_global(global: Option<String>) -> Self {
        let server = FakeServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = Context {
            memory_dir: dir.path().join("memory"),
            state_file: dir.path().join(".recall-state.json"),
            scopes: recall_paths::scope::scopes("acme/app".into(), global),
            source_env: "test".into(),
            client: Client::new(&server.url, "token").unwrap(),
        };
        Self { dir, server, ctx }
    }

    /// The absolute path of a memory file named relative to the memory
    /// directory.
    pub(crate) fn memory(&self, rel: &str) -> PathBuf {
        state::join_relative(&self.ctx.memory_dir, rel)
    }
}

pub(crate) fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}
