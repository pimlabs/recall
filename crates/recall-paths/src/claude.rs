//! Facts about Claude Code itself — where it keeps auto-memory, and how it
//! names the per-project directory.
//!
//! These are reverse-engineered from the installed CLI (v2.1.42), not a
//! published contract; see docs/phase-0-findings.md. They live in their own
//! module precisely because they track someone else's implementation: when
//! Claude Code changes, there is exactly one place to fix, with its own
//! tests, instead of the assumption being smeared across the codebase.

use std::path::{Path, PathBuf};

/// The subset of the environment these paths depend on. Passed in rather
/// than read from `std::env` inside, so the derivation is testable without
/// mutating process state — which, under Rust's parallel test threads,
/// would race every other test in the binary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Env {
    /// `CLAUDE_CODE_REMOTE_MEMORY_DIR`. When set it wins outright — and note
    /// that merely having it set is also what enables auto-memory at all in
    /// a remote session (findings §5).
    pub remote_memory_dir: Option<String>,
    /// `CLAUDE_CONFIG_DIR`.
    pub config_dir: Option<String>,
    /// `$HOME`.
    pub home: Option<String>,
}

impl Env {
    /// Reads the three variables through a caller-supplied lookup.
    pub fn from_lookup<F>(lookup: F) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        Env {
            remote_memory_dir: lookup("CLAUDE_CODE_REMOTE_MEMORY_DIR"),
            config_dir: lookup("CLAUDE_CONFIG_DIR"),
            home: lookup("HOME"),
        }
    }

    /// Reads the three variables from the real process environment.
    pub fn from_process_env() -> Self {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// The root Claude Code keeps per-project data under, in the same
    /// precedence order the CLI uses:
    ///
    /// `CLAUDE_CODE_REMOTE_MEMORY_DIR` > `CLAUDE_CONFIG_DIR` > `$HOME/.claude`
    pub fn memory_root(&self) -> PathBuf {
        if let Some(dir) = set(&self.remote_memory_dir) {
            return PathBuf::from(dir);
        }
        if let Some(dir) = set(&self.config_dir) {
            return PathBuf::from(dir);
        }
        Path::new(self.home.as_deref().unwrap_or("")).join(".claude")
    }

    /// Where Claude Code reads and writes auto-memory for the project rooted
    /// at `project_root`: `<root>/projects/<slug>/memory`.
    pub fn memory_dir(&self, project_root: &str) -> PathBuf {
        self.project_dir(project_root).join("memory")
    }

    /// Recall's own bookkeeping for delete reconciliation. It sits *beside*
    /// the memory directory rather than inside it, so it can never be
    /// mistaken for a memory file and pushed to the server.
    pub fn state_file(&self, project_root: &str) -> PathBuf {
        self.project_dir(project_root).join(".recall-state.json")
    }

    fn project_dir(&self, project_root: &str) -> PathBuf {
        self.memory_root().join("projects").join(slug(project_root))
    }
}

/// A variable that is present but empty is not set, matching how the CLI's
/// own reads behave (and how Go's `os.Getenv` reports both cases).
fn set(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|v| !v.is_empty())
}

/// Reproduces Claude Code's own project-directory naming, which in the CLI
/// is the JavaScript expression:
///
/// ```text
/// path.replace(/[^a-zA-Z0-9]/g, "-")
/// ```
///
/// The subtlety worth stating: a JavaScript regex replace operates on UTF-16
/// code units, so "é" (one code unit) becomes one dash while "🚀" (a
/// surrogate pair, two code units) becomes two. Iterating this string's
/// bytes would give two and four; iterating its `chars()` would give one and
/// one. Both diverge for any non-ASCII path — and the shell implementation
/// this replaces did exactly that, byte-wise and locale-dependent, so it
/// computed a directory Claude Code never writes to and sync silently did
/// nothing. Encoding to UTF-16 first is what makes this match the real
/// thing; it is not decoration.
pub fn slug(path: &str) -> String {
    // A UTF-8 string is never shorter in bytes than it is in UTF-16 code
    // units, so this is an upper bound on the output length.
    let mut out = String::with_capacity(path.len());
    for unit in path.encode_utf16() {
        match u8::try_from(unit) {
            // Retained units are ASCII by construction, so the cast back is
            // lossless.
            Ok(byte) if byte.is_ascii_alphanumeric() => out.push(char::from(byte)),
            _ => out.push('-'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The expected values here are what the real Claude Code CLI produces —
    /// its JS is `path.replace(/[^a-zA-Z0-9]/g, "-")`, which replaces per
    /// UTF-16 code unit. The non-ASCII rows are the ones the previous shell
    /// implementation got wrong (byte-wise, and differently depending on
    /// `LC_ALL`), which meant a wrong memory directory and silently dead
    /// sync.
    ///
    /// These rows are illustrative, not the whole proof: equivalence was also
    /// checked differentially against real `node` over 300 random paths mixing
    /// ASCII, accents, CJK, emoji and symbols — identical output on every one.
    /// Worth redoing that if this function is ever touched, since a table of
    /// hand-picked cases is exactly what a subtly wrong rewrite still passes.
    #[test]
    fn slugs_paths_the_way_claude_code_does() {
        for (input, want, why) in [
            (
                "/home/user/recall",
                "-home-user-recall",
                "real project path, verified against ~/.claude/projects on disk",
            ),
            (
                "/Users/eko/code/recall",
                "-Users-eko-code-recall",
                "macOS-style home path",
            ),
            (
                "/Users/Eko/MyRepo",
                "-Users-Eko-MyRepo",
                "case is preserved, only non-alphanumerics are replaced",
            ),
            (
                "/tmp/my project.v2",
                "-tmp-my-project-v2",
                "spaces and dots each become one dash",
            ),
            (
                "/home/weird|dir",
                "-home-weird-dir",
                "pipe character — broke the shell version's sed outright",
            ),
            (
                "/home/café",
                "-home-caf-",
                "latin-1 accent is one UTF-16 unit, so one dash",
            ),
            (
                "/home/🚀app",
                "-home---app",
                "astral char is a surrogate pair, so two dashes",
            ),
            ("", "", "empty stays empty"),
        ] {
            assert_eq!(slug(input), want, "{why}: slug({input:?})");
        }
    }

    /// Guards the two ways this is tempting to "simplify": byte iteration
    /// and `chars()`. Both would pass every ASCII row above.
    #[test]
    fn slug_counts_utf16_units_not_bytes_or_chars() {
        assert_eq!(slug("é"), "-", "one UTF-16 unit, but two UTF-8 bytes");
        assert_eq!(slug("🚀"), "--", "one char, but two UTF-16 units");
    }

    #[test]
    fn memory_root_follows_the_cli_precedence() {
        for (env, want, why) in [
            (
                Env {
                    remote_memory_dir: Some("/data/claude".into()),
                    config_dir: Some("/cfg".into()),
                    home: Some("/home/u".into()),
                },
                "/data/claude",
                "remote memory dir wins over everything",
            ),
            (
                Env {
                    config_dir: Some("/cfg".into()),
                    home: Some("/home/u".into()),
                    ..Default::default()
                },
                "/cfg",
                "config dir wins over home",
            ),
            (
                Env {
                    home: Some("/home/u".into()),
                    ..Default::default()
                },
                "/home/u/.claude",
                "falls back to ~/.claude",
            ),
            (
                Env {
                    remote_memory_dir: Some(String::new()),
                    config_dir: Some(String::new()),
                    home: Some("/home/u".into()),
                },
                "/home/u/.claude",
                "set-but-empty is not set",
            ),
        ] {
            assert_eq!(env.memory_root(), Path::new(want), "{why}");
        }
    }

    #[test]
    fn derives_memory_dir_and_state_file() {
        let env = Env {
            home: Some("/home/user".into()),
            ..Default::default()
        };
        let root = "/home/user/recall";

        assert_eq!(
            env.memory_dir(root),
            Path::new("/home/user/.claude/projects/-home-user-recall/memory")
        );
        assert_eq!(
            env.state_file(root),
            Path::new("/home/user/.claude/projects/-home-user-recall/.recall-state.json")
        );
    }

    /// Beside the memory dir, never inside it — otherwise the push hook
    /// would try to sync its own bookkeeping file as if it were a memory
    /// note.
    #[test]
    fn state_file_is_a_sibling_of_the_memory_dir() {
        let env = Env {
            home: Some("/home/user".into()),
            ..Default::default()
        };
        let root = "/home/user/recall";

        let state = env.state_file(root);
        assert_ne!(state.parent(), Some(env.memory_dir(root).as_path()));
        assert_eq!(state.parent(), env.memory_dir(root).parent());
    }

    #[test]
    fn reads_its_three_variables_through_the_lookup() {
        let env = Env::from_lookup(|key| match key {
            "CLAUDE_CODE_REMOTE_MEMORY_DIR" => Some("/data/claude".into()),
            "HOME" => Some("/home/u".into()),
            _ => None,
        });
        assert_eq!(
            env,
            Env {
                remote_memory_dir: Some("/data/claude".into()),
                config_dir: None,
                home: Some("/home/u".into()),
            }
        );
    }
}
