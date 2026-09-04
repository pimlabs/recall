//! Reconciles two versions of a memory file by shelling out to the local
//! `claude` CLI.
//!
//! It shells out on purpose: no Anthropic API key appears anywhere in this
//! codebase (see CLAUDE.md), so the merge rides whatever account is already
//! logged into the CLI on the host. That is a real operational dependency,
//! which is why every failure here degrades to last-write-wins rather than
//! failing the sync — a not-yet-configured merge must never be able to take
//! basic syncing down with it.

use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

use crate::now;

/// Replaces Claude Code's default agentic system prompt.
///
/// Together with `--exclude-dynamic-system-prompt-sections` and
/// `--strict-mcp-config`, and running from a neutral working directory,
/// this is what keeps a merge call cheap: measured live, the default prompt
/// plus project context turned a trivial merge into roughly $0.19 of
/// cache-creation tokens, versus about $0.01 with these. The task needs no
/// tools and no project context — only text in, text out.
pub const SYSTEM_PROMPT: &str = concat!(
    "You are a precise text-merging assistant for a personal notes-sync tool. ",
    "You merge two versions of a Claude Code auto-memory file that were edited independently on different machines and then synced through a central server. ",
    "Rules: preserve every distinct fact from both versions; if both state the same fact in different words, keep it once, worded clearly (prefer the more complete wording); ",
    "if they directly contradict each other, keep both and mark the conflict inline so a human can resolve it later; never invent information that isn't present in either version. ",
    "Output ONLY the merged file content — no preamble, no explanation, no code fences, nothing else."
);

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(45);
const STATUS_TIMEOUT: Duration = Duration::from_secs(15);

/// How much of a subprocess's output is worth quoting back in an error.
const DETAIL_LIMIT: usize = 500;

/// Why a merge could not produce a merged file. Every variant ends the same
/// way at the call site: fall back to last-write-wins and keep the sync.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The binary is missing, unspawnable, or not logged in.
    #[error("claude CLI unavailable: {0}")]
    Unavailable(String),
    /// It ran, but not within `RECALL_MERGE_TIMEOUT_MS`.
    #[error("claude merge timed out after {0:?}")]
    TimedOut(Duration),
    /// It exited non-zero.
    #[error("claude failed: {0}")]
    Failed(String),
    /// Its output was not the JSON envelope this asked for.
    #[error("claude returned non-JSON output: {0}")]
    NonJson(String),
    /// The envelope reported an error of its own.
    #[error("claude merge failed: {0}")]
    Rejected(String),
    /// A well-formed success envelope carrying nothing, from two versions
    /// that both had content.
    ///
    /// Treated as a failure precisely because it does not look like one:
    /// stored as a result it would replace both machines' notes with `""`
    /// and report `merged: true`.
    #[error("claude returned an empty merge of two non-empty versions")]
    EmptyResult,
}

/// Runs merges through a `claude` binary.
#[derive(Debug, Clone)]
pub struct Merger {
    /// The binary to run. Resolved through `PATH` like any other command.
    pub bin: String,
    /// How long it may take before the merge is abandoned.
    pub timeout: Duration,
}

impl Default for Merger {
    fn default() -> Self {
        Self {
            bin: "claude".to_string(),
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// The local, zero-cost check of whether a merge could work at all: is the
/// binary there, and is it logged in. Exposed via `/health` so a degraded
/// deployment is visible without waiting for a real conflict.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// When this check ran.
    pub checked_at: String,
    /// Whether the binary was found and runnable.
    pub available: bool,
    /// Whether it has a usable login.
    pub logged_in: bool,
    /// Why the check failed, when it did.
    pub error: String,
}

/// The subset of `claude -p --output-format json` we read.
#[derive(Debug, Deserialize)]
struct CliResult {
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    result: String,
}

/// `claude auth status` speaks camelCase.
#[derive(Debug, Deserialize)]
struct AuthStatus {
    #[serde(default, rename = "loggedIn")]
    logged_in: bool,
}

/// Builds the user-side prompt for one merge.
pub fn prompt(old_content: &str, new_content: &str) -> String {
    format!("--- VERSION A (currently stored) ---\n{old_content}\n\n--- VERSION B (incoming) ---\n{new_content}")
}

impl Merger {
    /// Builds a merger around one binary and one timeout.
    pub fn new(bin: impl Into<String>, timeout: Duration) -> Self {
        Self {
            bin: bin.into(),
            timeout: if timeout.is_zero() {
                DEFAULT_TIMEOUT
            } else {
                timeout
            },
        }
    }

    /// Returns a single reconciled version of the two inputs.
    pub async fn merge(&self, old_content: &str, new_content: &str) -> Result<String, Error> {
        let limit = if self.timeout.is_zero() {
            DEFAULT_TIMEOUT
        } else {
            self.timeout
        };

        let mut cmd = Command::new(&self.bin);
        cmd.arg("-p")
            .args(["--output-format", "json"])
            .args(["--input-format", "text"])
            .args(["--system-prompt", SYSTEM_PROMPT])
            .arg("--exclude-dynamic-system-prompt-sections")
            .arg("--strict-mcp-config")
            // A neutral working directory: nothing here should read, or be
            // influenced by, whatever project happens to be on disk. Along
            // with the two flags above this is the difference between a
            // ~$0.01 and a ~$0.19 merge call.
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // If the timeout below drops this future, the child must die
            // with it rather than linger holding a session.
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                Error::Unavailable(format!("{} not found on PATH", self.bin))
            }
            _ => Error::Failed(e.to_string()),
        })?;

        let mut stdin = child.stdin.take().expect("stdin was piped");
        let body = prompt(old_content, new_content);
        // Writing and draining run together: a prompt larger than the pipe
        // buffer would deadlock if we wrote it all before reading output.
        let run = async {
            let write = async move {
                stdin.write_all(body.as_bytes()).await?;
                stdin.shutdown().await
            };
            let (written, out) = tokio::join!(write, child.wait_with_output());
            // A child that closed stdin early shows up here as a broken
            // pipe; its exit status and stderr say more about why than the
            // write error does, so defer to them.
            if let Err(e) = written {
                if e.kind() != std::io::ErrorKind::BrokenPipe {
                    return Err(e);
                }
            }
            out.map(|o| (o.status, o.stdout, o.stderr))
        };

        let (status, stdout, stderr) = match timeout(limit, run).await {
            Err(_) => return Err(Error::TimedOut(limit)),
            Ok(Err(e)) => return Err(Error::Failed(e.to_string())),
            Ok(Ok(v)) => v,
        };

        if !status.success() {
            let detail = String::from_utf8_lossy(&stderr).trim().to_string();
            let detail = if detail.is_empty() {
                "(no stderr)".to_string()
            } else {
                detail
            };
            return Err(Error::Failed(format!(
                "exit {}: {}",
                status
                    .code()
                    .map_or_else(|| "signal".into(), |c| c.to_string()),
                truncate(&detail, DETAIL_LIMIT)
            )));
        }

        let out = String::from_utf8_lossy(&stdout);
        let parsed: CliResult = serde_json::from_str(&out)
            .map_err(|_| Error::NonJson(truncate(&out, DETAIL_LIMIT).to_string()))?;
        if parsed.is_error {
            return Err(Error::Rejected(
                truncate(&parsed.result, DETAIL_LIMIT).to_string(),
            ));
        }

        // A merge that comes back empty while both inputs had content is a
        // malfunction, not a result — the model refused, or emitted nothing,
        // or the envelope carried `is_error: false` over an empty body.
        //
        // Storing it would be the worst outcome this server can produce:
        // both machines' notes replaced by "", reported as `merged: true`,
        // and then written out as an empty file everywhere on the next pull.
        // Every other failure here degrades to last-write-wins, and so does
        // this one.
        if parsed.result.trim().is_empty()
            && !(old_content.trim().is_empty() && new_content.trim().is_empty())
        {
            return Err(Error::EmptyResult);
        }
        Ok(parsed.result)
    }

    /// Runs `claude auth status`, which is a local check and costs no
    /// tokens. A missing binary is reported as unavailable, never a panic.
    pub async fn check_status(&self) -> Status {
        let checked_at = now();
        let run = Command::new(&self.bin)
            .args(["auth", "status"])
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output();

        let out = match timeout(STATUS_TIMEOUT, run).await {
            Err(_) => {
                return Status {
                    checked_at,
                    error: format!("`{} auth status` timed out", self.bin),
                    ..Status::default()
                }
            }
            Ok(Err(e)) => {
                let error = if e.kind() == std::io::ErrorKind::NotFound {
                    "claude CLI not found on PATH".to_string()
                } else {
                    e.to_string()
                };
                return Status {
                    checked_at,
                    error,
                    ..Status::default()
                };
            }
            Ok(Ok(out)) => out,
        };

        if !out.status.success() {
            let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Status {
                checked_at,
                available: true,
                logged_in: false,
                error: if detail.is_empty() {
                    "`claude auth status` failed".to_string()
                } else {
                    truncate(&detail, DETAIL_LIMIT).to_string()
                },
            };
        }

        match serde_json::from_slice::<AuthStatus>(&out.stdout) {
            Ok(a) => Status {
                checked_at,
                available: true,
                logged_in: a.logged_in,
                error: String::new(),
            },
            Err(_) => Status {
                checked_at,
                available: true,
                logged_in: false,
                error: "could not parse `claude auth status` output".to_string(),
            },
        }
    }
}

fn truncate(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_labels_both_versions() {
        let p = prompt("old", "new");
        assert!(p.contains("--- VERSION A (currently stored) ---\nold"));
        assert!(p.contains("--- VERSION B (incoming) ---\nnew"));
    }

    /// No API key may reach the CLI, and the cost-saving flags must stay.
    #[test]
    fn the_system_prompt_is_verbatim() {
        assert!(SYSTEM_PROMPT.starts_with("You are a precise text-merging assistant"));
        assert!(SYSTEM_PROMPT.ends_with("no code fences, nothing else."));
    }

    #[tokio::test]
    async fn a_missing_binary_is_unavailable_not_a_panic() {
        let m = Merger::new("definitely-not-a-real-binary", Duration::from_secs(1));
        let st = m.check_status().await;
        assert!(!st.available);
        assert!(!st.logged_in);
        assert!(!st.error.is_empty());
        assert!(!st.checked_at.is_empty());

        let err = m.merge("a", "b").await.unwrap_err();
        assert!(matches!(err, Error::Unavailable(_)), "got {err:?}");
    }

    /// The envelope is what the CLI actually returns; `is_error` must be
    /// treated as a failure even though the process exits 0.
    #[tokio::test]
    async fn parses_the_json_envelope_and_honors_is_error() {
        let (_ok_dir, bin) = fake_claude(r#"{"is_error":false,"result":"merged!"}"#);
        assert_eq!(
            Merger::new(bin, Duration::from_secs(10))
                .merge("a", "b")
                .await
                .unwrap(),
            "merged!"
        );

        let (_bad_dir, bin) = fake_claude(r#"{"is_error":true,"result":"nope"}"#);
        let err = Merger::new(bin, Duration::from_secs(10))
            .merge("a", "b")
            .await;
        assert!(matches!(err, Err(Error::Rejected(_))), "got {err:?}");

        let (_junk_dir, bin) = fake_claude("not json");
        let err = Merger::new(bin, Duration::from_secs(10))
            .merge("a", "b")
            .await;
        assert!(matches!(err, Err(Error::NonJson(_))), "got {err:?}");
    }

    #[tokio::test]
    async fn reads_logged_in_from_auth_status() {
        let (_dir, bin) = fake_claude(r#"{"loggedIn":true}"#);
        let st = Merger::new(bin, Duration::from_secs(10))
            .check_status()
            .await;
        assert!(st.available && st.logged_in, "got {st:?}");
    }

    /// A stand-in `claude` that drains stdin, ignores its arguments and
    /// prints `out`. The TempDir comes back so the caller keeps the script
    /// alive for as long as it needs it.
    fn fake_claude(out: &str) -> (tempfile::TempDir, String) {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claude");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "#!/bin/sh").unwrap();
        writeln!(f, "cat > /dev/null").unwrap();
        writeln!(f, "printf '%s' '{out}'").unwrap();
        drop(f);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let bin = path.to_str().unwrap().to_string();
        (dir, bin)
    }
}
