//! The environment a hook sees, which is not the environment a shell sees.
//!
//! Claude Code applies the `env` block of every settings file in scope to
//! the processes it spawns, and an entry there *replaces* the value the
//! shell exported rather than deferring to it. So a project that declares
//! `RECALL_PROJECT_KEY` in its committed `.claude/settings.json` syncs under
//! that key, while `recall status` — typed by hand into a shell Claude Code
//! never touched — used to read the process environment and report a
//! different one. A diagnostic that disagrees with the thing it diagnoses is
//! the worst kind of bug, so this module exists to make every command that
//! resolves configuration resolve it the way the hooks do.
//!
//! What it deliberately does not model: managed (enterprise) settings and
//! command-line overrides, the two layers above these. Recall is a
//! single-owner tool with no fleet to be managed by, and inventing platform
//! paths that nothing here can test would trade a known gap for an unknown
//! one. The gap is stated rather than hidden: a machine under managed
//! settings can still be reported wrongly, and this is the honest place to
//! say so.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use recall_paths::claude;

use crate::settings;

/// Reads one environment variable.
///
/// Boxed rather than a generic parameter so [`Environment`] stays a plain
/// type that callers can hold and pass around, and so the shell can be
/// supplied by a test — a test that set real environment variables would
/// race every other test in the binary.
pub type Shell = Box<dyn Fn(&str) -> Option<String>>;

/// One variable that a settings file declares, as `recall status` reports
/// it. Carries no value: `RECALL_TOKEN` is one of these, and a diagnostic
/// that prints your token is not a diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Declared {
    /// The variable's name.
    pub name: String,
    /// The settings file it was declared in.
    pub file: String,
    /// Whether the same variable is *also* set in this shell, to a different
    /// value. The file wins, so the shell value is not in effect — which is
    /// precisely the disagreement someone would otherwise spend an afternoon
    /// on.
    pub shadows_shell: bool,
    /// Whether it was declared as an empty string. Recall treats empty as
    /// unset, so such a declaration turns the setting off *and* hides
    /// whatever the shell had — the two failures compounding, silently.
    pub empty: bool,
}

struct Entry {
    value: String,
    file: PathBuf,
}

/// The layered environment: every settings file in scope, over the shell.
///
/// Built lowest-precedence-first, so a later layer simply overwrites an
/// earlier one — the same rule Claude Code documents for these files.
pub struct Environment {
    declared: BTreeMap<String, Entry>,
    unreadable: Vec<String>,
    shell: Shell,
}

impl Environment {
    /// Reads the real settings files for a project, over the real process
    /// environment.
    ///
    /// The order is Claude Code's, lowest precedence first: the user-level
    /// `settings.json`, then the project's committed `.claude/settings.json`,
    /// then its untracked `.claude/settings.local.json`.
    pub fn discover(project_root: &Path) -> Self {
        let project = project_root.join(".claude");
        Self::from_files(
            &[
                claude::Env::from_process_env().user_settings_file(),
                project.join("settings.json"),
                project.join("settings.local.json"),
            ],
            Box::new(|name| std::env::var(name).ok()),
        )
    }

    /// The same, with the files and the shell supplied — which is what makes
    /// this testable at all, since a test that set real environment
    /// variables would race every other test in the binary.
    ///
    /// `files` is ordered lowest precedence first. A file that does not
    /// exist is not a finding; one that exists and cannot be read or parsed
    /// is, and lands in [`Environment::unreadable`].
    ///
    /// A path listed twice is read once. Two layers *can* be the same file —
    /// a repository whose root is the home directory, a dotfiles project,
    /// makes the user-level and project-level paths identical — and without
    /// this a single broken settings file would be reported twice in a
    /// diagnostic whose whole job is to be believed.
    pub fn from_files(files: &[PathBuf], shell: Shell) -> Self {
        let mut env = Environment {
            declared: BTreeMap::new(),
            unreadable: Vec::new(),
            shell,
        };

        let mut seen = BTreeSet::new();
        for file in files {
            if !seen.insert(file.as_path()) {
                continue;
            }
            let src = match fs::read(file) {
                Ok(src) => src,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(_) => {
                    env.unreadable.push(file.display().to_string());
                    continue;
                }
            };
            match settings::env_block(&src) {
                Ok(vars) => {
                    for (name, value) in vars {
                        env.declared.insert(
                            name,
                            Entry {
                                value,
                                file: file.clone(),
                            },
                        );
                    }
                }
                Err(_) => env.unreadable.push(file.display().to_string()),
            }
        }
        env
    }

    /// The value a hook would see for `name`.
    ///
    /// A declaration wins outright, empty string included — that is what
    /// "replaces the value inherited from the shell" means, and treating an
    /// empty declaration as a fall-through would invent a rule Claude Code
    /// does not have.
    pub fn get(&self, name: &str) -> Option<String> {
        match self.declared.get(name) {
            Some(entry) => Some(entry.value.clone()),
            None => (self.shell)(name),
        }
    }

    /// This environment as the lookup closure
    /// [`ClientConfig::from_lookup`](recall_paths::ClientConfig::from_lookup)
    /// takes, so every command resolves configuration through one code path.
    pub fn lookup(&self) -> impl Fn(&str) -> Option<String> + '_ {
        |name| self.get(name)
    }

    /// Which of `names` a settings file declares, in the order given.
    ///
    /// Filtered to the variables Recall actually reads: an `env` block is a
    /// general-purpose thing and may hold a dozen entries that are none of
    /// Recall's business, and listing those would be both noise and a way to
    /// leak the shape of someone's setup into a pasted bug report.
    pub fn declared(&self, names: &[&str]) -> Vec<Declared> {
        names
            .iter()
            .filter_map(|name| {
                let entry = self.declared.get(*name)?;
                Some(Declared {
                    name: (*name).to_string(),
                    file: entry.file.display().to_string(),
                    shadows_shell: (self.shell)(name)
                        .is_some_and(|from_shell| from_shell != entry.value),
                    empty: entry.value.is_empty(),
                })
            })
            .collect()
    }

    /// Settings files that exist but could not be read, or held something
    /// that is not a JSON object.
    ///
    /// Reported rather than swallowed because Claude Code cannot read them
    /// either: the hooks are running with none of what the file was meant to
    /// declare, and the file being *present* is what makes that invisible.
    pub fn unreadable(&self) -> &[String] {
        &self.unreadable
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn shell(pairs: &[(&str, &str)]) -> Shell {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        Box::new(move |name| map.get(name).cloned())
    }

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        path
    }

    /// The bug this module exists for: the shell says one thing, the
    /// committed settings file says another, and the hooks obey the file.
    #[test]
    fn a_declaration_beats_the_shell() {
        let dir = tempfile::tempdir().unwrap();
        let file = write(
            dir.path(),
            "settings.json",
            r#"{"env":{"RECALL_PROJECT_KEY":"acme/app"}}"#,
        );

        let env = Environment::from_files(
            &[file],
            shell(&[
                ("RECALL_PROJECT_KEY", "wrong/key"),
                ("RECALL_URL", "https://recall.example.com"),
            ]),
        );

        assert_eq!(env.get("RECALL_PROJECT_KEY").as_deref(), Some("acme/app"));
        assert_eq!(
            env.get("RECALL_URL").as_deref(),
            Some("https://recall.example.com"),
            "a variable no file declares still comes from the shell"
        );
        assert_eq!(env.get("RECALL_TOKEN"), None);

        let declared = env.declared(&["RECALL_URL", "RECALL_PROJECT_KEY"]);
        assert_eq!(
            declared,
            vec![Declared {
                name: "RECALL_PROJECT_KEY".to_string(),
                file: dir.path().join("settings.json").display().to_string(),
                shadows_shell: true,
                empty: false,
            }],
            "only the declared variable is reported, and it is reported as \
             shadowing the shell"
        );
    }

    /// Setting the same value in both places is not a conflict worth
    /// shouting about — and saying so would train people to ignore the line
    /// that matters.
    #[test]
    fn an_identical_value_in_both_places_does_not_read_as_shadowing() {
        let dir = tempfile::tempdir().unwrap();
        let file = write(
            dir.path(),
            "settings.json",
            r#"{"env":{"RECALL_PROJECT_KEY":"acme/app"}}"#,
        );

        let env = Environment::from_files(&[file], shell(&[("RECALL_PROJECT_KEY", "acme/app")]));
        assert!(!env.declared(&["RECALL_PROJECT_KEY"])[0].shadows_shell);
    }

    /// Lowest precedence first, so `settings.local.json` wins — and the
    /// report has to name the file that actually won, or it sends someone
    /// editing the wrong one.
    #[test]
    fn the_local_override_wins_and_is_named_as_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let user = write(
            dir.path(),
            "user.json",
            r#"{"env":{"RECALL_URL":"https://user.example.com","RECALL_TOKEN":"t"}}"#,
        );
        let shared = write(
            dir.path(),
            "settings.json",
            r#"{"env":{"RECALL_URL":"https://shared.example.com"}}"#,
        );
        let local = write(
            dir.path(),
            "settings.local.json",
            r#"{"env":{"RECALL_URL":"https://local.example.com"}}"#,
        );

        let env = Environment::from_files(&[user, shared, local], shell(&[]));
        assert_eq!(
            env.get("RECALL_URL").as_deref(),
            Some("https://local.example.com")
        );
        assert_eq!(
            env.get("RECALL_TOKEN").as_deref(),
            Some("t"),
            "a lower layer still supplies what no higher one declares"
        );
        assert!(env.declared(&["RECALL_URL"])[0]
            .file
            .ends_with("settings.local.json"));
    }

    /// An empty declaration is the compound failure: Recall reads empty as
    /// unset, so the setting is off, *and* the shell value that would have
    /// worked is hidden behind it.
    #[test]
    fn an_empty_declaration_is_kept_and_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let file = write(
            dir.path(),
            "settings.json",
            r#"{"env":{"RECALL_GLOBAL_KEY":""}}"#,
        );

        let env = Environment::from_files(&[file], shell(&[("RECALL_GLOBAL_KEY", "eko")]));
        assert_eq!(env.get("RECALL_GLOBAL_KEY").as_deref(), Some(""));

        let declared = &env.declared(&["RECALL_GLOBAL_KEY"])[0];
        assert!(declared.empty);
        assert!(declared.shadows_shell);
    }

    /// A file Claude Code cannot parse is one whose declarations are not in
    /// effect for the hooks either. Silence there would report the shell's
    /// values as if they were the hooks', which is the original bug wearing
    /// a different hat.
    #[test]
    fn a_file_that_is_not_json_is_reported_rather_than_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let broken = write(dir.path(), "settings.json", "{ not json");
        let array = write(dir.path(), "settings.local.json", "[]");

        let env = Environment::from_files(&[broken, array], shell(&[]));
        assert_eq!(env.unreadable().len(), 2);
        assert!(env.unreadable()[0].ends_with("settings.json"));
    }

    /// A project rooted at the home directory makes two of the three layers
    /// the same path. Reporting one broken file twice would make the report
    /// look like the bug.
    #[test]
    fn the_same_file_in_two_layers_is_read_once() {
        let dir = tempfile::tempdir().unwrap();
        let broken = write(dir.path(), "settings.json", "{ not json");

        let env = Environment::from_files(&[broken.clone(), broken], shell(&[]));
        assert_eq!(env.unreadable().len(), 1);
    }

    /// The ordinary case on almost every machine: no settings file at all,
    /// or one with no `env` block. Neither is a finding.
    #[test]
    fn absent_files_and_env_less_files_are_not_findings() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_only = write(
            dir.path(),
            "settings.json",
            r#"{"hooks":{"PostToolUse":[]}}"#,
        );

        let env = Environment::from_files(
            &[dir.path().join("absent.json"), hooks_only],
            shell(&[("RECALL_URL", "https://recall.example.com")]),
        );

        assert!(env.unreadable().is_empty());
        assert!(env.declared(&["RECALL_URL"]).is_empty());
        assert_eq!(
            env.get("RECALL_URL").as_deref(),
            Some("https://recall.example.com")
        );
    }
}
