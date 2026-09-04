//! What `recall push` and `recall pull` actually do.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use recall_wire::PushRequest;

use crate::atomic;
use crate::state;
use crate::syncclient::Client;

/// Everything the hooks need to know about where they're running.
///
/// Passed in rather than discovered inside, so the logic is testable without
/// a real home directory or a real git repository — and so path derivation
/// stays somebody else's problem.
#[derive(Debug)]
pub struct Env {
    pub memory_dir: PathBuf,
    pub state_file: PathBuf,
    pub project_key: String,
    pub source_env: String,
    pub client: Client,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("pushing {path}: {source}")]
    Push {
        path: String,
        #[source]
        source: crate::syncclient::Error,
    },
    #[error("pushing delete of {path}: {source}")]
    PushDelete {
        path: String,
        #[source]
        source: crate::syncclient::Error,
    },
    #[error("pulling {project_key}: {source}")]
    Pull {
        project_key: String,
        #[source]
        source: crate::syncclient::Error,
    },
    /// A memory file that isn't valid UTF-8 can't cross the wire: `content`
    /// is a JSON string. Refusing is louder than the alternative, but the
    /// alternative is silent corruption — Go's `json.Marshal` replaces
    /// invalid bytes with U+FFFD and pushes the result as if it were the
    /// file.
    #[error("{path} is not valid UTF-8, so it cannot be synced as a memory file")]
    NotUtf8 { path: String },
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// What a push actually did, for logging and tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushResult {
    /// The memory-file path that was sent, if the triggering file was one.
    pub pushed: Option<String>,
    /// Paths reported as deleted by reconciliation.
    pub deleted: Vec<String>,
    /// The triggering file wasn't a memory file, so nothing happened at all.
    pub skipped: bool,
}

/// Handles one `PostToolUse` invocation.
///
/// Two things happen here, and only one of them is about the file that
/// triggered the hook. The triggering file is pushed if it is a memory file.
/// Separately — and regardless of what triggered this run — the memory
/// directory is reconciled against the last known baseline, and anything
/// that vanished is reported as a delete. That reconciliation is the *only*
/// mechanism that catches deletes at all: Claude Code has no delete event,
/// and an `rm` through the Bash tool wouldn't match an `Edit|Write` matcher
/// even if it did.
pub async fn push(env: &Env, triggered_path: &Path) -> Result<PushResult, Error> {
    let mut res = PushResult::default();

    if triggered_path.as_os_str().is_empty() || !is_under(&env.memory_dir, triggered_path) {
        // Not a memory file. Nothing to push, and — importantly — no
        // reconciliation either: this hook fires on every Edit and Write in
        // the session, and a directory walk plus a state write on each one
        // would be pure waste. Deletes still propagate on the next edit that
        // does touch a memory file.
        res.skipped = true;
        return Ok(res);
    }

    let baseline = state::load(&env.state_file)?;

    // Reconcile deletes, but never on the very first run for a project.
    // With no baseline, an empty or partial memory directory would otherwise
    // read as "everything was deleted" and tombstone the project's whole
    // history on the server. `None` here means "nothing has ever synced",
    // which is why load() distinguishes it from an empty baseline.
    if let Some(prev) = baseline {
        for rel in &prev.files {
            if state::join_relative(&env.memory_dir, rel).exists() {
                continue;
            }
            let req = PushRequest {
                project_key: env.project_key.clone(),
                file_path: rel.clone(),
                deleted: true,
                source_env: env.source_env.clone(),
                ..Default::default()
            };
            env.client
                .push(&req)
                .await
                .map_err(|source| Error::PushDelete {
                    path: rel.clone(),
                    source,
                })?;
            res.deleted.push(rel.clone());
        }
    }

    if fs::metadata(triggered_path).is_ok_and(|m| !m.is_dir()) {
        let rel =
            relative_slash(&env.memory_dir, triggered_path).expect("containment was just checked");

        // Exact bytes. The shell version used command substitution, which
        // strips every trailing newline, so a file with none or with two came
        // back from a round trip with exactly one — content silently altered.
        // Reading raw and handing the bytes straight to the serializer is
        // what prevents that.
        let content = fs::read(triggered_path)?;
        let content =
            String::from_utf8(content).map_err(|_| Error::NotUtf8 { path: rel.clone() })?;

        let req = PushRequest {
            project_key: env.project_key.clone(),
            file_path: rel.clone(),
            content,
            source_env: env.source_env.clone(),
            deleted: false,
        };
        env.client.push(&req).await.map_err(|source| Error::Push {
            path: rel.clone(),
            source,
        })?;
        res.pushed = Some(rel);
    }

    refresh_state(env)?;
    Ok(res)
}

/// What a pull changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PullResult {
    pub written: Vec<String>,
    pub removed: Vec<String>,
}

impl PullResult {
    /// A short line for the hook's stderr, which is where Claude Code shows
    /// hook output to the user.
    pub fn describe(&self, project_key: &str) -> String {
        format!(
            "recall-pull: synced {} memory file(s), removed {} deleted file(s) for {}",
            self.written.len(),
            self.removed.len(),
            project_key
        )
    }
}

/// Fetches the server's state and makes the local memory directory match it,
/// then refreshes the baseline so a machine that only ever pulls still has an
/// accurate one — otherwise its first local delete would go unnoticed.
pub async fn pull(env: &Env) -> Result<PullResult, Error> {
    let mut res = PullResult::default();

    let resp = env
        .client
        .pull(&env.project_key)
        .await
        .map_err(|source| Error::Pull {
            project_key: env.project_key.clone(),
            source,
        })?;

    if resp.files.is_empty() {
        // The server has nothing for this project yet. There is nothing to
        // write and nothing to reconcile against, and writing a baseline
        // here would only invent one out of whatever happens to be on disk.
        return Ok(res);
    }

    fs::create_dir_all(&env.memory_dir)?;

    for file in &resp.files {
        // The server validates this on the way in, and it is validated again
        // here on the way out: this is the moment a malicious or buggy
        // server's traversal path would become a write outside the memory
        // directory on *this* machine. A bad path is skipped rather than
        // failing the pull, so one poisoned row can't block the rest.
        if recall_wire::validate_file_path(&file.file_path).is_err() {
            continue;
        }
        let dest = state::join_relative(&env.memory_dir, &file.file_path);
        // Belt and braces: validation is the real guard, but the containment
        // check is cheap and this is the security boundary.
        if !is_under(&env.memory_dir, &dest) {
            continue;
        }

        if file.deleted {
            match fs::remove_file(&dest) {
                Ok(()) => res.removed.push(file.file_path.clone()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            continue;
        }

        // A tombstone withholds content; an empty file legitimately has
        // content that happens to be empty. `Option` is what keeps those
        // apart, so `None` here means "nothing to write", not "write ''".
        let Some(content) = file.content.as_ref() else {
            continue;
        };

        atomic::write(&dest, ".recall-", ".tmp", content.as_bytes())?;
        res.written.push(file.file_path.clone());
    }

    refresh_state(env)?;
    Ok(res)
}

fn refresh_state(env: &Env) -> Result<(), Error> {
    let files = state::list_memory_files(&env.memory_dir)?;
    state::save(&env.state_file, &files)?;
    Ok(())
}

/// Reports whether `path` sits inside `dir`.
///
/// Compared segment-wise after a lexical clean, so `/a/memory-notes` is
/// correctly *not* treated as inside `/a/memory`. A plain string-prefix check
/// gets that wrong, and the shell version did.
fn is_under(dir: &Path, path: &Path) -> bool {
    relative_slash(dir, path).is_some()
}

/// The slash-separated path of `path` relative to `dir`, or `None` if it
/// isn't strictly inside it (equal counts as not inside).
fn relative_slash(dir: &Path, path: &Path) -> Option<String> {
    let dir = lexical_clean(dir);
    let path = lexical_clean(path);
    let rel = path.strip_prefix(&dir).ok()?;

    let mut out = String::new();
    for component in rel.components() {
        let Component::Normal(segment) = component else {
            // A `..` or a root that survived cleaning means this isn't a
            // plain descendant, whatever the prefix match said.
            return None;
        };
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(&segment.to_string_lossy());
    }
    if out.is_empty() {
        return None;
    }
    Some(out)
}

/// Resolves `.` and `..` textually, without touching the filesystem —
/// the equivalent of Go's `filepath.Clean`. Purely lexical is what we want:
/// the memory directory may not exist yet on a first pull.
fn lexical_clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // Popping past the root is a no-op, as `/..` is `/`.
                if !out.pop() && !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testserver::FakeServer;
    use recall_wire::File;

    struct Fixture {
        _dir: tempfile::TempDir,
        server: FakeServer,
        env: Env,
    }

    impl Fixture {
        async fn new() -> Self {
            let server = FakeServer::start().await;
            let dir = tempfile::tempdir().unwrap();
            let env = Env {
                memory_dir: dir.path().join("memory"),
                state_file: dir.path().join(".recall-state.json"),
                project_key: "acme/app".into(),
                source_env: "test".into(),
                client: Client::new(&server.url, "token").unwrap(),
            };
            Self {
                _dir: dir,
                server,
                env,
            }
        }

        fn memory(&self, rel: &str) -> PathBuf {
            state::join_relative(&self.env.memory_dir, rel)
        }
    }

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[tokio::test]
    async fn push_ignores_files_outside_the_memory_dir() {
        let f = Fixture::new().await;
        let other = f._dir.path().join("src").join("main.rs");
        write(&other, "fn main() {}");

        let res = push(&f.env, &other).await.unwrap();
        assert!(res.skipped, "a non-memory file should be skipped");
        assert!(f.server.pushes().is_empty());
        // A skipped run must not write a baseline either — doing so on an
        // unrelated edit would record an empty directory as the truth.
        assert!(
            !f.env.state_file.exists(),
            "state file written for a skipped push"
        );
    }

    /// `/a/memory-notes` must not count as inside `/a/memory`.
    #[tokio::test]
    async fn push_ignores_a_sibling_directory_sharing_a_name_prefix() {
        let f = Fixture::new().await;
        let mut sneaky = f.env.memory_dir.clone().into_os_string();
        sneaky.push("-notes");
        let sneaky = PathBuf::from(sneaky).join("NOTES.md");
        write(&sneaky, "not memory");

        let res = push(&f.env, &sneaky).await.unwrap();
        assert!(
            res.skipped,
            "a sibling directory sharing a name prefix was treated as inside the memory dir"
        );
        assert!(f.server.pushes().is_empty());
    }

    #[test]
    fn containment_is_segment_wise() {
        let dir = Path::new("/a/memory");
        for inside in [
            "/a/memory/MEMORY.md",
            "/a/memory/topics/auth.md",
            "/a/./memory/MEMORY.md",
            "/a/memory/topics/../MEMORY.md",
        ] {
            assert!(
                is_under(dir, Path::new(inside)),
                "{inside} should be inside"
            );
        }
        for outside in [
            "/a/memory-notes/x.md",
            "/a/memoryx",
            "/a/memory",
            "/a/other/MEMORY.md",
            "/a/memory/../escaped.md",
            "/",
        ] {
            assert!(
                !is_under(dir, Path::new(outside)),
                "{outside} should be outside"
            );
        }
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

            push(&f.env, &path).await.unwrap();

            let pushes = f.server.pushes();
            assert_eq!(pushes.len(), 1, "{name}: expected exactly one push");
            assert_eq!(
                pushes[0].content, content,
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
        push(&f.env, &path).await.unwrap();

        let pushes = f.server.pushes();
        assert_eq!(pushes.len(), 1);
        assert!(!pushes[0].deleted);
        assert_eq!(pushes[0].content, "");
    }

    /// A file Claude names on the fly is pushed exactly like `MEMORY.md` —
    /// there is no list of known filenames anywhere in this path.
    #[tokio::test]
    async fn push_catches_a_dynamically_named_topic_file() {
        let f = Fixture::new().await;
        let path = f.memory("debugging.md");
        write(&path, "# Debugging\n");

        let res = push(&f.env, &path).await.unwrap();
        assert_eq!(res.pushed.as_deref(), Some("debugging.md"));
    }

    #[tokio::test]
    async fn push_sends_nested_paths_with_forward_slashes() {
        let f = Fixture::new().await;
        let path = f.memory("topics/auth.md");
        write(&path, "# Auth\n");

        push(&f.env, &path).await.unwrap();
        assert_eq!(f.server.pushes()[0].file_path, "topics/auth.md");
    }

    #[tokio::test]
    async fn push_sends_the_project_key_and_source_env() {
        let f = Fixture::new().await;
        let path = f.memory("MEMORY.md");
        write(&path, "x");
        push(&f.env, &path).await.unwrap();

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

        let res = push(&f.env, &path).await.unwrap();
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
            state::load(&f.env.state_file).unwrap().is_none(),
            "precondition: no baseline"
        );

        let res = push(&f.env, &path).await.unwrap();
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
        push(&f.env, &memory).await.unwrap();
        fs::remove_file(&gone).unwrap();

        // A later edit to an unrelated memory file is what carries the delete.
        let res = push(&f.env, &memory).await.unwrap();
        assert_eq!(res.deleted, vec!["gone.md"]);
        assert!(
            f.server
                .pushes()
                .iter()
                .any(|p| p.deleted && p.file_path == "gone.md"),
            "no tombstone push was sent for the deleted file"
        );

        // And the baseline no longer lists it, so it isn't re-reported forever.
        let baseline = state::load(&f.env.state_file).unwrap().expect("a baseline");
        assert!(
            !baseline.files.iter().any(|f| f == "gone.md"),
            "deleted file still listed in the refreshed baseline"
        );

        // A third run has nothing left to reconcile.
        let res = push(&f.env, &memory).await.unwrap();
        assert!(res.deleted.is_empty(), "delete re-reported on a later run");
    }

    #[tokio::test]
    async fn push_reconciles_a_nested_delete() {
        let f = Fixture::new().await;
        let memory = f.memory("MEMORY.md");
        let nested = f.memory("topics/auth.md");
        write(&memory, "# Memory\n");
        write(&nested, "# Auth\n");

        push(&f.env, &memory).await.unwrap();
        fs::remove_file(&nested).unwrap();

        let res = push(&f.env, &memory).await.unwrap();
        assert_eq!(res.deleted, vec!["topics/auth.md"]);
    }

    #[tokio::test]
    async fn push_surfaces_a_server_rejection() {
        let f = Fixture::new().await;
        let path = f.memory("MEMORY.md");
        write(&path, "# Memory\n");
        f.server.fail_with(401, r#"{"error":"unauthorized"}"#);

        let err = push(&f.env, &path).await.unwrap_err();
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

        let err = push(&f.env, &path).await.unwrap_err();
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

        let res = pull(&f.env).await.unwrap();
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

        let res = pull(&f.env).await.unwrap();
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

        let res = pull(&f.env).await.unwrap();
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
        f.env.memory_dir = f._dir.path().join("a").join("b").join("memory");
        let outside_root = f._dir.path().to_path_buf();
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

        let res = pull(&f.env).await.unwrap();
        assert!(res.written.is_empty(), "wrote {:?}", res.written);
        for escaped in [
            outside_root.join("a").join("b").join("escaped.md"),
            outside_root.join("a").join("escaped.md"),
            outside_root.join("escaped.md"),
            PathBuf::from("/etc/passwd.recall-test"),
        ] {
            assert!(
                !escaped.exists(),
                "a file was written outside the memory directory: {}",
                escaped.display()
            );
        }
        assert!(
            state::list_memory_files(&f.env.memory_dir)
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

        let res = pull(&f.env).await.unwrap();
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

            pull(&f.env).await.unwrap();
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

        pull(&f.env).await.unwrap();
        let baseline = state::load(&f.env.state_file)
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
        pull(&f.env).await.unwrap();

        fs::remove_file(f.memory("obsolete.md")).unwrap();
        let res = push(&f.env, &f.memory("MEMORY.md")).await.unwrap();
        assert_eq!(res.deleted, vec!["obsolete.md"]);
    }

    #[tokio::test]
    async fn pull_surfaces_a_server_rejection() {
        let f = Fixture::new().await;
        f.server.fail_with(500, "boom");
        let err = pull(&f.env).await.unwrap_err();
        assert!(matches!(err, Error::Pull { .. }), "got {err:?}");
        assert!(err.to_string().contains("500"), "{err}");
    }

    /// An empty response leaves the disk alone rather than inventing a
    /// baseline out of whatever happens to be there.
    #[tokio::test]
    async fn pull_with_nothing_on_the_server_is_a_no_op() {
        let f = Fixture::new().await;
        let res = pull(&f.env).await.unwrap();
        assert_eq!(res, PullResult::default());
        assert!(!f.env.state_file.exists());
        assert!(!f.env.memory_dir.exists());
    }

    #[test]
    fn describes_a_pull() {
        let res = PullResult {
            written: vec!["MEMORY.md".into()],
            removed: vec!["a.md".into(), "b.md".into()],
        };
        assert_eq!(
            res.describe("acme/app"),
            "recall-pull: synced 1 memory file(s), removed 2 deleted file(s) for acme/app"
        );
    }
}
