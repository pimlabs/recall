//! `recall status` — the command people run when something is wrong, so it
//! has to work when everything is wrong.
//!
//! Nothing here is allowed to fail the process: an unset variable, a dead
//! server and a project that was never wired are all *findings*, reported in
//! the output, not errors.

use recall_hooks::config::Source;
use recall_hooks::declared_env::{Declared, Ignored};
use recall_hooks::{
    claude, client::Client, config, exit, project, scope, settings, state, ClientConfig,
};

use crate::project as proj;

/// Every variable Recall reads, in the order status reports them.
///
/// Assembled from the two crates that own the reads rather than retyped, so
/// a variable added there cannot be silently missing here — which would
/// reintroduce, one variable at a time, exactly the blind spot this report
/// was fixed to remove.
fn known_vars() -> Vec<&'static str> {
    config::VARS
        .iter()
        .chain(claude::VARS.iter())
        .copied()
        .collect()
}

/// How the project's key was arrived at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeySource {
    /// `RECALL_PROJECT_KEY`, and it was usable.
    Declared,
    /// Derived from the git remote — the ordinary case.
    Remote,
    /// No remote, so derived from this checkout's path. Two machines will
    /// disagree; see `RECALL_PROJECT_KEY`.
    LocalPath,
    /// `RECALL_PROJECT_KEY` was set but rejected, so the key is derived.
    DeclaredButRejected,
}

/// A directory at the memory root that names a reserved one in the wrong
/// case.
///
/// Both halves are carried rather than the offending name alone, so the text
/// report and the JSON say the same thing without either of them working out
/// the answer a second time.
#[derive(serde::Serialize)]
pub struct MiscasedDir {
    /// The name on disk, e.g. `Global`.
    pub found: String,
    /// The name it would have to be, e.g. `global`.
    pub reserved: &'static str,
}

/// A variable in the environment winning over a different value in
/// `config.toml`. Never the token: this is printed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Override {
    /// The variable, e.g. `RECALL_SOURCE_ENV`.
    pub variable: &'static str,
    /// Its value in the environment, which is the one in effect.
    pub environment: String,
    /// The setting in `config.toml` it overrides, e.g. `machine.name`.
    pub setting: &'static str,
    /// That setting's value.
    pub config: String,
}

/// The `--json` shape. Stable enough to script against; that is the point of
/// having it at all.
#[derive(serde::Serialize)]
pub struct Report {
    /// The project root this ran in.
    pub project: String,
    /// The key it syncs under.
    pub project_key: String,
    /// Where that key came from.
    ///
    /// Worth reporting on its own: a `RECALL_PROJECT_KEY` that is set but
    /// unusable falls back to the derived key rather than failing, so
    /// without this the only symptom is memory quietly syncing to a
    /// different bucket than the one you asked for.
    pub project_key_source: KeySource,
    /// Where Claude Code keeps this project's memory on this machine.
    pub memory_dir: String,
    /// How many memory files are on disk right now.
    pub memory_files: usize,
    /// Whether `.claude/settings.json` carries Recall's hooks.
    pub hooks_wired: bool,
    /// Whether the working directory is inside a git repository at all.
    ///
    /// Not the same question as [`Report::project_key_source`], which says
    /// where the key came from: a repository with no remote and a plain
    /// directory both derive one from the path. The difference matters to
    /// `recall doctor`, because unwired hooks in a project are a problem and
    /// unwired hooks in your home directory are just where you are standing.
    pub in_git_repo: bool,
    /// Whether this is a remote or cloud session, per `CLAUDE_CODE_REMOTE`.
    ///
    /// The same signal `.claude/hooks/session-start.sh` keys off. It decides
    /// whether an unset `CLAUDE_CODE_REMOTE_MEMORY_DIR` is correct or fatal,
    /// which is otherwise unanswerable from here — and being unanswerable is
    /// how it ended up a permanent warning in both places.
    pub remote_session: bool,
    /// Variables a settings file declares, which is where their value
    /// actually comes from — the shell's is replaced, not consulted. Absent
    /// from the JSON when there are none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub declared_env: Vec<Declared>,
    /// Variables a settings file names but could not set, because the
    /// value was not a string. Absent from the JSON when there are none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ignored_env: Vec<Ignored>,
    /// Settings files that exist but are not readable JSON. Claude Code
    /// cannot read them either, so nothing they declare is in effect.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unreadable_settings: Vec<String>,
    /// The key global memories sync under, when global sync is on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub global_key: Option<String>,
    /// Variables that were set to a value Recall refused, so the setting did
    /// nothing. Absent from the JSON when there are none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rejected_vars: Vec<&'static str>,
    /// How many global memory files are on disk here.
    pub global_files: usize,
    /// The key machine memories sync under, when a machine scope is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_key: Option<String>,
    /// How many machine memory files are on disk here.
    pub machine_files: usize,
    /// Whether `MEMORY.md` links the machine index. Reported separately from
    /// the count for the same reason as the global one: files being present
    /// and Claude Code being able to reach them are different questions, and
    /// the machine scope shipped answering only the first.
    pub machine_linked: bool,
    /// A directory at the memory root naming a reserved one in the wrong
    /// case, if there is one. Nothing under it syncs, and on macOS it looks
    /// like the real thing, so it is worth saying out loud.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub miscased_dir: Option<MiscasedDir>,
    /// Whether `MEMORY.md` links the global index. Without that link Claude
    /// Code never reads any of it, so it is worth reporting separately from
    /// "the files are here".
    pub global_linked: bool,
    /// Whether `CLAUDE_CODE_REMOTE_MEMORY_DIR` is set.
    ///
    /// Unset is correct on a laptop and fatal in a remote session, where
    /// Claude Code's auto-memory is off entirely without it — so this is
    /// reported rather than judged here, and `recall doctor` is where the
    /// two cases are told apart.
    pub remote_memory_dir_set: bool,
    /// Whether `RECALL_URL` is set.
    pub url_set: bool,
    /// Whether `RECALL_TOKEN` is set.
    pub token_set: bool,
    /// Where the URL came from: the environment, or the credentials file
    /// `recall connect` writes.
    pub url_source: Source,
    /// Where the token came from. When it is the environment,
    /// `declared_env` says whether a settings file or the shell supplied it.
    pub token_source: Source,
    /// The credentials file, when it was consulted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credentials_file: Option<String>,
    /// Why the credentials file could not be used, when it exists and
    /// could not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credentials_error: Option<String>,
    /// `~/.recall/config.toml`, when there is a `~/.recall` to look in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_file: Option<String>,
    /// Where the machine scope's key came from: `RECALL_MACHINE_KEY`, or the
    /// machine name in `config.toml`.
    pub machine_source: Source,
    /// Settings in `config.toml` that were read and did nothing — an
    /// unknown key, or a machine name that cannot be used.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub config_problems: Vec<String>,
    /// Variables in the environment that override a *different* value in
    /// `config.toml`. The environment wins, which is right in a cloud
    /// session and usually a leftover on a laptop that has since run
    /// `recall connect`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub overridden: Vec<Override>,
    /// Whether anyone but its owner can read the credentials file.
    /// `recall connect` never writes one like that; a copy or a restore can.
    pub credentials_exposed: bool,
    /// Whether `GET /health` answered.
    pub server_ok: bool,
    /// Why it didn't, when it didn't.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_error: Option<String>,
    /// The commit the server was built from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<String>,
    /// Whether the server can actually merge, or is silently falling back to
    /// last-write-wins.
    pub merge_ready: bool,
    /// How many live files the server holds for this project.
    pub synced_files: usize,
    /// When any project last synced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_synced_at: Option<String>,
    /// When a copy last reached somewhere the loss of the server does not
    /// reach. [`None`] when nothing has ever written the stamp, which is
    /// also what "no off-box backup is configured" looks like.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_offbox_at: Option<String>,
}

/// Collects the report, then prints it as text or JSON.
pub async fn run(as_json: bool) -> anyhow::Result<i32> {
    let here = proj::resolve();
    let cfg = here.config();
    let rep = collect(&here, &cfg).await;

    if as_json {
        println!("{}", serde_json::to_string_pretty(&rep)?);
    } else {
        print_text(&cfg, &rep);
    }
    Ok(exit::OK)
}

/// Gathers the whole report.
///
/// Shared with `recall doctor` rather than collected twice. Every finding
/// doctor reports is a reading of this struct, so a question added here
/// reaches both commands at once — and, more to the point, one added here
/// cannot be silently missing from doctor, which is the shape of bug this
/// crate has now shipped twice.
pub(crate) async fn collect(here: &proj::Resolved, cfg: &ClientConfig) -> Report {
    let root = &here.root;
    let root_str = root.to_string_lossy().to_string();
    let remote = proj::remote();
    let memory_dir = here.memory_dir();

    let mut rep = Report {
        project: root_str.clone(),
        project_key: here.project_key(cfg, &remote),
        project_key_source: key_source(cfg, &remote),
        memory_dir: memory_dir.display().to_string(),
        memory_files: state::list_memory_files(&memory_dir)
            .map(|f| f.len())
            .unwrap_or(0),
        hooks_wired: std::fs::read(root.join(".claude").join("settings.json"))
            .map(|b| settings::is_wired(&b))
            .unwrap_or(false),
        in_git_repo: proj::git_root().is_some(),
        // Read from the process environment rather than through
        // `here.env.lookup()`: this one describes the harness the command is
        // running under, and a settings file claiming otherwise would be
        // describing something it cannot know.
        remote_session: proj::remote_session(),
        declared_env: here.env.declared(&known_vars()),
        ignored_env: here.env.ignored(&known_vars()),
        unreadable_settings: here.env.unreadable().to_vec(),
        global_key: cfg.global_key.clone(),
        rejected_vars: cfg.rejected_vars.clone(),
        global_files: state::list_memory_files(&memory_dir.join(scope::GLOBAL_DIR))
            .map(|f| f.len())
            .unwrap_or(0),
        // Only the memory root is read, not the whole tree: the reserved
        // name is reserved at the root, so a `Global/` three levels down is
        // an ordinary directory and routes to the project correctly.
        machine_key: cfg.machine_key.clone(),
        machine_files: state::list_memory_files(&memory_dir.join(scope::MACHINE_DIR))
            .map(|f| f.len())
            .unwrap_or(0),
        machine_linked: std::fs::read(memory_dir.join("MEMORY.md"))
            .map(|b| recall_hooks::machine_index_is_linked(&b))
            .unwrap_or(false),
        miscased_dir: std::fs::read_dir(&memory_dir).ok().and_then(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().is_dir())
                .find_map(|e| {
                    let found = e.file_name().to_string_lossy().into_owned();
                    scope::miscased_reserved_dir(&found).map(|(_, reserved)| MiscasedDir {
                        found: found.clone(),
                        reserved,
                    })
                })
        }),
        global_linked: std::fs::read(memory_dir.join("MEMORY.md"))
            .map(|b| recall_hooks::global_index_is_linked(&b))
            .unwrap_or(false),
        remote_memory_dir_set: claude::Env::from_lookup(here.env.lookup())
            .remote_memory_dir
            .is_some_and(|d| !d.is_empty()),
        url_set: !cfg.url.is_empty(),
        token_set: !cfg.token.is_empty(),
        url_source: cfg.url_source,
        token_source: cfg.token_source,
        credentials_file: cfg
            .credentials_file
            .as_ref()
            .map(|p| p.display().to_string()),
        credentials_error: cfg.credentials_error.clone(),
        credentials_exposed: cfg
            .credentials_file
            .as_deref()
            .is_some_and(recall_hooks::home::readable_by_others),
        config_file: cfg.config_file.as_ref().map(|p| p.display().to_string()),
        machine_source: cfg.machine_source,
        config_problems: cfg.config_problems.clone(),
        overridden: overrides(here, cfg),
        server_ok: false,
        server_error: None,
        git_commit: None,
        merge_ready: false,
        synced_files: 0,
        last_synced_at: None,
        last_offbox_at: None,
    };

    if !rep.url_set {
        return rep;
    }

    match Client::new(&cfg.url, &cfg.token) {
        Ok(client) => {
            match client.health().await {
                Ok(health) => {
                    rep.server_ok = true;
                    rep.git_commit = Some(health.git_commit);
                    rep.merge_ready = health.merge.claude_cli.logged_in.unwrap_or(false);
                    if !health.last_sync_at.is_empty() {
                        rep.last_synced_at = Some(health.last_sync_at);
                    }
                    if !health.last_offbox_at.is_empty() {
                        rep.last_offbox_at = Some(health.last_offbox_at);
                    }
                }
                Err(err) => rep.server_error = Some(err.to_string()),
            }
            if rep.token_set {
                if let Ok(resp) = client.pull(&rep.project_key).await {
                    rep.synced_files = resp.files.iter().filter(|f| !f.deleted).count();
                }
            }
        }
        Err(err) => rep.server_error = Some(err.to_string()),
    }
    rep
}

/// Variables in the environment that override a different value in
/// `config.toml`.
///
/// Compared as each value is *used*, not as it was typed: `jarvis` and
/// `machine:jarvis` are the same machine key, and `https://x/` is the same
/// server as `https://x`, so neither is reported as an override.
fn overrides(here: &proj::Resolved, cfg: &ClientConfig) -> Vec<Override> {
    let env = |name: &str| here.env.get(name).filter(|v| !v.trim().is_empty());
    let mut out = Vec::new();

    if let (Some(url), Some(saved)) = (env("RECALL_URL"), cfg.saved_server.as_deref()) {
        if recall_hooks::home::normalize_url(&url) != recall_hooks::home::normalize_url(saved) {
            out.push(Override {
                variable: "RECALL_URL",
                environment: url,
                setting: "server",
                config: saved.to_string(),
            });
        }
    }
    let saved_name = cfg
        .saved_machine_name
        .as_deref()
        .and_then(recall_hooks::home::machine_name);
    if let Some(name) = saved_name.as_deref() {
        if let Some(raw) = env("RECALL_MACHINE_KEY") {
            if scope::machine_key(&raw) != scope::machine_key(name) {
                out.push(Override {
                    variable: "RECALL_MACHINE_KEY",
                    environment: raw,
                    setting: "machine.name",
                    config: name.to_string(),
                });
            }
        }
        if let Some(label) = env("RECALL_SOURCE_ENV") {
            if label.trim() != name {
                out.push(Override {
                    variable: "RECALL_SOURCE_ENV",
                    environment: label,
                    setting: "machine.name",
                    config: name.to_string(),
                });
            }
        }
    }
    out
}

/// Distinguishes "you did not declare a key" from "you declared one and it
/// was thrown away", which look identical in the key itself.
fn key_source(cfg: &ClientConfig, remote: &str) -> KeySource {
    if cfg.project_key.is_some() {
        return KeySource::Declared;
    }
    if cfg.rejected_vars.contains(&"RECALL_PROJECT_KEY") {
        return KeySource::DeclaredButRejected;
    }
    if project::key_from_remote(remote).is_some() {
        KeySource::Remote
    } else {
        KeySource::LocalPath
    }
}

/// Which settings file declares `name`, if one does.
fn declared_in<'a>(rep: &'a Report, name: &str) -> Option<&'a str> {
    rep.declared_env
        .iter()
        .find(|var| var.name == name)
        .map(|var| var.file.as_str())
}

/// Whether a settings file declares `name` as an empty string.
fn declared_empty(rep: &Report, name: &str) -> bool {
    rep.declared_env
        .iter()
        .any(|var| var.name == name && var.empty)
}

/// The block that exists because a value set in a settings file and a value
/// exported from a shell look identical once they are in the environment —
/// and only one of them is the one the hooks obey.
fn print_declared_env(rep: &Report) {
    for (i, var) in rep.declared_env.iter().enumerate() {
        // Continuation lines are indented to the width of the labels above,
        // so a multi-variable block reads as one answer rather than four.
        let label = if i == 0 {
            "declared env "
        } else {
            "             "
        };
        let mut note = String::new();
        if var.empty {
            note.push_str(" — declared EMPTY, so the setting is off");
        }
        if var.shadows_shell {
            note.push_str(if var.empty {
                ", and it hides the value set in this shell"
            } else {
                " — this overrides the value set in this shell"
            });
        }
        println!("{label}: {} from {}{note}", var.name, var.file);
    }

    for var in &rep.ignored_env {
        println!(
            "settings     : {} in {} is not a string, so it sets nothing",
            var.name, var.file
        );
    }

    for file in &rep.unreadable_settings {
        println!("settings     : UNREADABLE — {file}");
    }
    if !rep.unreadable_settings.is_empty() {
        println!(
            "               Claude Code cannot read it either, so nothing it \
declares is in effect for the hooks."
        );
    }
}

fn print_text(cfg: &ClientConfig, rep: &Report) {
    println!("project      : {}", rep.project);
    // Bound rather than inlined: one arm has to name the settings file the
    // key came from, and a `format!` inside a `match` inside a `println!`
    // does not outlive the statement that borrows it.
    let key_source = match rep.project_key_source {
        KeySource::Declared => format!(
            "declared in RECALL_PROJECT_KEY, {}",
            match declared_in(rep, "RECALL_PROJECT_KEY") {
                Some(file) => format!("set by {file}"),
                None => "set in this shell".to_string(),
            }
        ),
        KeySource::Remote => "from the git remote".to_string(),
        KeySource::LocalPath => {
            "from this checkout's path — no git remote, so another machine will \
             disagree; set RECALL_PROJECT_KEY on both"
                .to_string()
        }
        KeySource::DeclaredButRejected => {
            "RECALL_PROJECT_KEY was SET BUT UNUSABLE and ignored — it must be \
             non-empty, free of whitespace, and not under 'global:'"
                .to_string()
        }
    };
    // An empty declaration never reaches `rejected_vars` — an empty value
    // reads as unset before anything can refuse it — so without this the
    // line would report a derived key while a settings file three lines
    // below is plainly trying to set it.
    let key_source = match declared_in(rep, "RECALL_PROJECT_KEY") {
        Some(file) if declared_empty(rep, "RECALL_PROJECT_KEY") => format!(
            "{key_source}; RECALL_PROJECT_KEY is declared empty in {file}, which reads as unset"
        ),
        _ => key_source,
    };
    println!("project_key  : {} ({key_source})", rep.project_key);
    println!("memory dir   : {}", rep.memory_dir);
    println!("memory files : {} on disk", rep.memory_files);
    println!(
        "hooks wired  : {}",
        if rep.hooks_wired {
            "yes"
        } else {
            "NO — run 'recall init' in this project"
        }
    );
    print_declared_env(rep);
    println!(
        "global       : {}",
        match &rep.global_key {
            None if rep.rejected_vars.contains(&"RECALL_GLOBAL_KEY") =>
                "off — RECALL_GLOBAL_KEY was SET BUT EMPTY once trimmed, so it was ignored"
                    .to_string(),
            // An empty *declaration* never reaches `rejected_vars`: an empty
            // value reads as unset before anything gets a chance to refuse
            // it. Without this arm the line would advise setting a variable
            // that is already set, two lines under a report saying where it
            // was set and that it is empty.
            None if declared_empty(rep, "RECALL_GLOBAL_KEY") => format!(
                "off — RECALL_GLOBAL_KEY is declared empty in {}, which reads as unset",
                declared_in(rep, "RECALL_GLOBAL_KEY").unwrap_or("a settings file")
            ),
            None => "off (set RECALL_GLOBAL_KEY to share memories across projects)".to_string(),
            Some(key) if rep.global_linked => format!(
                "{key} — {} file(s), linked from MEMORY.md",
                rep.global_files
            ),
            Some(key) => format!(
                "{key} — {} file(s), NOT linked from MEMORY.md yet (run 'recall pull')",
                rep.global_files
            ),
        }
    );
    println!(
        "machine      : {}",
        match &rep.machine_key {
            None if rep.rejected_vars.contains(&"RECALL_MACHINE_KEY") =>
                "off — RECALL_MACHINE_KEY was SET BUT EMPTY once trimmed, so it was ignored"
                    .to_string(),
            None if declared_empty(rep, "RECALL_MACHINE_KEY") => format!(
                "off — RECALL_MACHINE_KEY is declared empty in {}, which reads as unset",
                declared_in(rep, "RECALL_MACHINE_KEY").unwrap_or("a settings file")
            ),
            // Deliberately not phrased as an invitation. The global line
            // suggests setting a key because sharing more is usually what
            // someone wants; this content is true of one machine only, and a
            // cloud session — a new machine every time — should leave it off.
            None => "off (set RECALL_MACHINE_KEY for memories about this machine only)".to_string(),
            Some(key) if rep.machine_files == 0 => format!("{key} — no files yet"),
            Some(key) if rep.machine_linked => {
                format!(
                    "{key} — {} file(s), linked from MEMORY.md",
                    rep.machine_files
                )
            }
            // The state this scope shipped in: the files arrive and Claude
            // Code never opens them, because it reads what MEMORY.md links.
            Some(key) => format!(
                "{key} — {} file(s), NOT linked from MEMORY.md yet (run 'recall pull')",
                rep.machine_files
            ),
        }
    );
    if let Some(d) = &rep.miscased_dir {
        println!(
            "             ! '{}/' is not '{}/', so nothing under it syncs. On \
             macOS the two are the same directory and on Linux they are not, so \
             Recall refuses rather than file it somewhere you did not mean. \
             Rename it to '{}'.",
            d.found, d.reserved, d.reserved
        );
    }
    let from_file = rep
        .credentials_file
        .as_deref()
        .unwrap_or("the credentials file");
    let from_config = rep.config_file.as_deref().unwrap_or("the config file");
    println!(
        "RECALL_URL   : {}",
        match rep.url_source {
            Source::Unset => "(unset)".to_string(),
            Source::Environment => cfg.url.clone(),
            Source::CredentialsFile | Source::ConfigFile => {
                format!("{} (from {from_config})", cfg.url)
            }
        }
    );
    println!(
        "RECALL_TOKEN : {}",
        match rep.token_source {
            Source::Unset => "(unset)".to_string(),
            Source::Environment => match declared_in(rep, "RECALL_TOKEN") {
                Some(file) => format!("set, by {file}"),
                None => "set, in this shell".to_string(),
            },
            Source::CredentialsFile | Source::ConfigFile => format!("saved in {from_file}"),
        }
    );
    for problem in &rep.config_problems {
        println!("config       : {problem} ({from_config})");
    }
    for o in &rep.overridden {
        println!(
            "config       : {}={} overrides {} = {:?} in {from_config}",
            o.variable, o.environment, o.setting, o.config
        );
    }
    if let Some(err) = &rep.credentials_error {
        println!("credentials  : UNREADABLE — {err}");
    }
    if rep.credentials_exposed {
        println!("credentials  : readable by other users — chmod 600 {from_file}");
    }

    if !rep.url_set {
        return;
    }
    if !rep.server_ok {
        println!(
            "server       : UNREACHABLE ({})",
            rep.server_error.as_deref().unwrap_or("unknown error")
        );
        return;
    }

    println!(
        "server       : reachable (git_commit {})",
        rep.git_commit.as_deref().unwrap_or("unknown")
    );
    println!(
        "merge        : {}",
        if rep.merge_ready {
            "ready (claude CLI logged in)"
        } else {
            "not configured — server falls back to last-write-wins"
        }
    );
    println!("synced files : {} on server", rep.synced_files);
}
