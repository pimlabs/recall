//! Facts about Claude Code itself — where it keeps auto-memory, and how it
//! names the per-project directory.
//!
//! These are reverse-engineered from the installed CLI (v2.1.42), not a
//! published contract; see docs/history/phase-0-findings.md. They live in their own
//! module precisely because they track someone else's implementation: when
//! Claude Code changes, there is exactly one place to fix, with its own
//! tests, instead of the assumption being smeared across the codebase.

use std::fs;
use std::path::{Path, PathBuf};

use crate::path::lexical_clean;

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
    /// `$HOME`, or `%USERPROFILE%` where that is unset — Windows has no
    /// `HOME` by default. **Not verified against a real Windows Claude Code
    /// install**: it is a reasonable guess (Node's `os.homedir()` falls back
    /// to `USERPROFILE` the same way), not a confirmed fact about what the
    /// CLI itself resolves. See docs/reference/install.md's Windows section.
    pub home: Option<String>,
}

/// Every variable [`Env`] reads.
///
/// Published so a caller that has to *enumerate* them — `recall status`,
/// reporting which of them a settings file declares — cannot drift from what
/// [`Env::from_lookup`] actually consults. The
/// `reads_exactly_the_variables_it_publishes` test holds the two together.
pub const VARS: &[&str] = &[
    "CLAUDE_CODE_REMOTE_MEMORY_DIR",
    "CLAUDE_CONFIG_DIR",
    "HOME",
    "USERPROFILE",
];

impl Env {
    /// Reads the four variables through a caller-supplied lookup.
    pub fn from_lookup<F>(lookup: F) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        Env {
            remote_memory_dir: lookup("CLAUDE_CODE_REMOTE_MEMORY_DIR"),
            config_dir: lookup("CLAUDE_CONFIG_DIR"),
            // An empty HOME reads as unset before USERPROFILE gets a turn,
            // matching how every other value here treats "set but empty".
            home: lookup("HOME")
                .filter(|v| !v.is_empty())
                .or_else(|| lookup("USERPROFILE")),
        }
    }

    /// Reads the four variables from the real process environment.
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

    /// Where Claude Code reads its *settings* from, which is deliberately
    /// not [`Env::memory_root`]:
    ///
    /// `CLAUDE_CONFIG_DIR` > `$HOME/.claude`
    ///
    /// `CLAUDE_CODE_REMOTE_MEMORY_DIR` is absent from that order on purpose.
    /// It moves where auto-memory is *kept* and nothing else — a remote
    /// session that sets it still reads its settings from the config
    /// directory — so letting it win here would send `recall status` looking
    /// for a settings file inside the memory directory.
    pub fn config_root(&self) -> PathBuf {
        if let Some(dir) = set(&self.config_dir) {
            return PathBuf::from(dir);
        }
        Path::new(self.home.as_deref().unwrap_or("")).join(".claude")
    }

    /// The user-level `settings.json`: the lowest-precedence settings file,
    /// and still above the shell.
    ///
    /// [`None`] when neither `CLAUDE_CONFIG_DIR` nor `$HOME` is set, rather
    /// than the bare `.claude/settings.json` that [`Env::config_root`] would
    /// otherwise produce. That path is *relative*, so it resolves against
    /// whatever directory the process happens to be in — which in a
    /// sub-directory of a project would load that sub-directory's settings
    /// as the user's, at the wrong precedence, under a second spelling of a
    /// file that may already be a layer.
    pub fn user_settings_file(&self) -> Option<PathBuf> {
        if set(&self.config_dir).is_none() && set(&self.home).is_none() {
            return None;
        }
        Some(self.config_root().join("settings.json"))
    }

    /// Recall's own bookkeeping for delete reconciliation. It sits *beside*
    /// the memory directory rather than inside it, so it can never be
    /// mistaken for a memory file and pushed to the server.
    pub fn state_file(&self, project_root: &str) -> PathBuf {
        self.project_dir(project_root).join(".recall-state.json")
    }

    /// `recall review`'s per-machine record of what it last found: beside
    /// [`Env::state_file`], for the same reason — recall's own bookkeeping,
    /// never a memory file, and never pushed.
    pub fn review_file(&self, project_root: &str) -> PathBuf {
        self.project_dir(project_root).join(".recall-review.json")
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

/// The directory Claude Code names a project's data after, given a checkout's
/// git root: that root, unless it is a linked worktree, which shares the
/// project directory of the repository it belongs to. So a session started in
/// a worktree reads and writes the main checkout's memory.
///
/// Read from the CLI (2.1.309) and compared with the `memory_paths` real
/// sessions report, for each shape in this module's tests. It is deliberately
/// not `git rev-parse --git-common-dir`, which disagrees with the CLI about a
/// worktree moved by hand and about a `.git` file naming a repository that
/// never made it. The CLI's other checks, on what a path may contain, are not
/// reproduced: they turn away paths no real repository has.
pub fn canonical_root(git_root: &Path) -> PathBuf {
    repository_of_worktree(git_root).unwrap_or_else(|| git_root.to_path_buf())
}

/// [`canonical_root`] for a linked worktree, [`None`] for anything else.
fn repository_of_worktree(git_root: &Path) -> Option<PathBuf> {
    // A directory, as in a main checkout, fails to read here.
    let gitfile = fs::read_to_string(git_root.join(".git")).ok()?;
    let private = gitfile.trim().strip_prefix("gitdir:")?.trim();
    let private = lexical_clean(&git_root.join(private));

    // A submodule has a `.git` file too, and no `commondir` beside it.
    let common = fs::read_to_string(private.join("commondir")).ok()?;
    let common = lexical_clean(&private.join(common.trim()));
    if private.parent()? != common.join("worktrees") {
        return None;
    }

    // `mv` leaves this naming where the worktree used to be, and the CLI
    // then treats it as a project of its own.
    let back = fs::read_to_string(private.join("gitdir")).ok()?;
    let back = fs::canonicalize(private.join(back.trim())).ok()?;
    if back != fs::canonicalize(git_root).ok()?.join(".git") {
        return None;
    }

    if common.file_name()? == ".git" {
        return common.parent().map(Path::to_path_buf);
    }
    // A bare repository has no main checkout, so its directory stands in.
    (!common.join(".git").exists()).then_some(common)
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

    /// What `git worktree add` leaves behind: a `.git` file in the worktree,
    /// and a directory under the repository's naming it back.
    fn link_worktree(common: &Path, worktree: &Path) {
        let private = common.join("worktrees").join("w");
        fs::create_dir_all(&private).unwrap();
        fs::create_dir_all(worktree).unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", private.display()),
        )
        .unwrap();
        fs::write(private.join("commondir"), "../..\n").unwrap();
        fs::write(
            private.join("gitdir"),
            format!("{}\n", worktree.join(".git").display()),
        )
        .unwrap();
    }

    /// A main checkout, and a directory with no git at all, are their own
    /// project. So is a subdirectory handed in by a caller that has no git
    /// root to give.
    #[test]
    fn a_checkout_that_is_not_a_linked_worktree_is_its_own_project() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        assert_eq!(canonical_root(dir.path()), dir.path());

        let plain = tempfile::tempdir().unwrap();
        assert_eq!(canonical_root(plain.path()), plain.path());
    }

    /// The case this exists for, wherever the worktree happens to live.
    #[test]
    fn a_linked_worktree_belongs_to_its_main_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        for worktree in [
            main.join(".claude").join("worktrees").join("x"),
            dir.path().join("elsewhere"),
        ] {
            link_worktree(&main.join(".git"), &worktree);
            assert_eq!(canonical_root(&worktree), main, "{}", worktree.display());
        }
    }

    /// `worktree.useRelativePaths` writes all three files relative to the
    /// directory each one sits in.
    #[test]
    fn a_worktree_with_relative_paths_is_followed_the_same() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        let worktree = main.join("wt");
        link_worktree(&main.join(".git"), &worktree);
        fs::write(worktree.join(".git"), "gitdir: ../.git/worktrees/w\n").unwrap();
        fs::write(main.join(".git/worktrees/w/gitdir"), "../../../wt/.git\n").unwrap();

        assert_eq!(canonical_root(&worktree), main);
    }

    /// A bare repository has no main checkout to name, so the CLI names its
    /// directory, `.bare` layout included. With a `.git` of its own inside,
    /// which no bare repository has, the worktree stands alone.
    #[test]
    fn a_worktree_of_a_bare_repository_belongs_to_that_directory() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["app.git", ".bare"] {
            let bare = dir.path().join(name);
            let worktree = dir.path().join("wt");
            link_worktree(&bare, &worktree);
            assert_eq!(canonical_root(&worktree), bare);

            fs::write(bare.join(".git"), "").unwrap();
            assert_eq!(canonical_root(&worktree), worktree);
        }
    }

    /// A submodule's `.git` file points into the superproject, with no
    /// `commondir` there, and it stays a project of its own.
    #[test]
    fn a_submodule_is_its_own_project() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("super/.git/modules/lib");
        let checkout = dir.path().join("super/lib");
        fs::create_dir_all(&module).unwrap();
        fs::create_dir_all(&checkout).unwrap();
        fs::write(
            checkout.join(".git"),
            format!("gitdir: {}\n", module.display()),
        )
        .unwrap();

        assert_eq!(canonical_root(&checkout), checkout);
    }

    /// A worktree moved by hand still works in git, and the CLI stops
    /// treating it as part of the repository, because the way back no longer
    /// names it. `git rev-parse --git-common-dir` would say otherwise.
    #[test]
    fn a_worktree_moved_by_hand_is_its_own_project() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        let worktree = dir.path().join("wt");
        link_worktree(&main.join(".git"), &worktree);
        let moved = dir.path().join("moved");
        fs::rename(&worktree, &moved).unwrap();

        assert_eq!(canonical_root(&moved), moved);
    }

    /// A `.git` file that merely claims a repository takes nothing from it:
    /// the repository's own `worktrees/` has to hold what it points at, or a
    /// checkout could put its memory under another project's name. git itself
    /// follows this one.
    #[test]
    fn a_gitfile_naming_a_repository_that_never_made_it_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim/.git");
        let claimed = dir.path().join("forged/fake");
        let forged = dir.path().join("forged");
        fs::create_dir_all(&victim).unwrap();
        fs::create_dir_all(&claimed).unwrap();
        fs::write(
            forged.join(".git"),
            format!("gitdir: {}\n", claimed.display()),
        )
        .unwrap();
        fs::write(claimed.join("commondir"), format!("{}\n", victim.display())).unwrap();
        fs::write(
            claimed.join("gitdir"),
            format!("{}\n", forged.join(".git").display()),
        )
        .unwrap();

        assert_eq!(canonical_root(&forged), forged);
    }

    /// Settings and memory are two different roots, and the variable that
    /// separates them is the one a cloud session sets. Getting this wrong
    /// would send `recall status` hunting for a settings file inside the
    /// memory directory and reporting, wrongly, that nothing is declared.
    #[test]
    fn settings_live_in_the_config_root_not_the_memory_root() {
        let remote = Env {
            remote_memory_dir: Some("/workspace/memory".into()),
            config_dir: None,
            home: Some("/home/eko".into()),
        };
        assert_eq!(remote.memory_root(), Path::new("/workspace/memory"));
        assert_eq!(remote.config_root(), Path::new("/home/eko/.claude"));
        assert_eq!(
            remote.user_settings_file().as_deref(),
            Some(Path::new("/home/eko/.claude/settings.json"))
        );

        // A relative path here would be read against the process's working
        // directory — a file belonging to whatever project the command was
        // run in, loaded as though it were the user's.
        let nowhere = Env {
            remote_memory_dir: Some("/workspace/memory".into()),
            config_dir: None,
            home: None,
        };
        assert_eq!(nowhere.user_settings_file(), None);

        let explicit = Env {
            remote_memory_dir: None,
            config_dir: Some("/cfg".into()),
            home: Some("/home/eko".into()),
        };
        assert_eq!(explicit.config_root(), Path::new("/cfg"));
    }

    /// [`VARS`] exists so callers can enumerate what is read. A list that
    /// drifts from the reads is worse than no list, so it is asserted
    /// against the reads themselves rather than maintained by hand.
    #[test]
    fn reads_exactly_the_variables_it_publishes() {
        use std::cell::RefCell;

        let seen = RefCell::new(Vec::new());
        let _ = Env::from_lookup(|key| {
            seen.borrow_mut().push(key.to_string());
            None
        });

        let mut seen = seen.into_inner();
        seen.sort();
        let mut published: Vec<String> = VARS.iter().map(|v| (*v).to_string()).collect();
        published.sort();
        assert_eq!(seen, published, "VARS no longer matches what Env reads");
    }

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

    /// **Not verified against a real Windows Claude Code install** — see the
    /// module doc and docs/reference/install.md's Windows section. This pins
    /// down what *is* known rather than leaving the assumption implicit:
    /// `git rev-parse --show-toplevel` prints forward slashes even on
    /// Windows (`project::root` uses it unchanged), and `slug` maps `:` and
    /// either slash character to one dash each, so a drive-letter path
    /// slugs the same way whichever separator it happens to use. What is
    /// genuinely unknown is whether Claude Code's own Node process computes
    /// the *same string* from the same directory — a lowercased drive
    /// letter, for one, would slug differently and point at a different
    /// memory directory.
    #[test]
    fn slugs_a_windows_style_path_the_same_regardless_of_separator() {
        for (input, why) in [
            (
                "C:/Users/eko/code/recall",
                "forward slashes, what git prints",
            ),
            (
                r"C:\Users\eko\code\recall",
                "backslashes, what Node's path APIs print",
            ),
        ] {
            assert_eq!(
                slug(input),
                "C--Users-eko-code-recall",
                "{why}: slug({input:?})"
            );
        }
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

    /// `recall review`'s state sits beside `.recall-state.json`, for the same
    /// reason: never inside the memory directory, so it is never mistaken
    /// for a memory file and pushed.
    #[test]
    fn review_file_is_beside_the_state_file() {
        let env = Env {
            home: Some("/home/user".into()),
            ..Default::default()
        };
        let root = "/home/user/recall";

        assert_eq!(
            env.review_file(root),
            Path::new("/home/user/.claude/projects/-home-user-recall/.recall-review.json")
        );
        assert_eq!(
            env.review_file(root).parent(),
            env.state_file(root).parent()
        );
    }

    #[test]
    fn reads_its_four_variables_through_the_lookup() {
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

    /// Windows has no `HOME` by default, so `USERPROFILE` has to stand in —
    /// see the module doc for how much of this is actually verified.
    #[test]
    fn home_falls_back_to_userprofile_when_home_is_unset_or_empty() {
        let env = Env::from_lookup(|key| match key {
            "USERPROFILE" => Some(r"C:\Users\eko".into()),
            _ => None,
        });
        assert_eq!(env.home.as_deref(), Some(r"C:\Users\eko"), "HOME unset");

        let env = Env::from_lookup(|key| match key {
            "HOME" => Some(String::new()),
            "USERPROFILE" => Some(r"C:\Users\eko".into()),
            _ => None,
        });
        assert_eq!(
            env.home.as_deref(),
            Some(r"C:\Users\eko"),
            "HOME declared empty reads as unset, same as everywhere else"
        );

        let env = Env::from_lookup(|key| match key {
            "HOME" => Some("/home/eko".into()),
            "USERPROFILE" => Some(r"C:\Users\eko".into()),
            _ => None,
        });
        assert_eq!(
            env.home.as_deref(),
            Some("/home/eko"),
            "HOME wins when both are set — this is the ordinary Unix case, \
             USERPROFILE is only a fallback"
        );
    }
}
