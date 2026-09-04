//! `recall status` — the command people run when something is wrong, so it
//! has to work when everything is wrong.
//!
//! Nothing here is allowed to fail the process: an unset variable, a dead
//! server and a project that was never wired are all *findings*, reported in
//! the output, not errors.

use recall_hooks::{client::Client, exit, settings, state};
use recall_paths::{project, scope, ClientConfig};

use crate::project as proj;

/// The `--json` shape. Stable enough to script against; that is the point of
/// having it at all.
#[derive(serde::Serialize)]
pub struct Report {
    /// The project root this ran in.
    pub project: String,
    /// The key it syncs under.
    pub project_key: String,
    /// Where Claude Code keeps this project's memory on this machine.
    pub memory_dir: String,
    /// How many memory files are on disk right now.
    pub memory_files: usize,
    /// Whether `.claude/settings.json` carries Recall's hooks.
    pub hooks_wired: bool,
    /// The key global memories sync under, when global sync is on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub global_key: Option<String>,
    /// How many global memory files are on disk here.
    pub global_files: usize,
    /// Whether `MEMORY.md` links the global index. Without that link Claude
    /// Code never reads any of it, so it is worth reporting separately from
    /// "the files are here".
    pub global_linked: bool,
    /// Whether `RECALL_URL` is set.
    pub url_set: bool,
    /// Whether `RECALL_TOKEN` is set.
    pub token_set: bool,
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
}

/// Collects the report, then prints it as text or JSON.
pub async fn run(as_json: bool) -> anyhow::Result<i32> {
    let cfg = ClientConfig::from_process_env();
    let rep = collect(&cfg).await;

    if as_json {
        println!("{}", serde_json::to_string_pretty(&rep)?);
    } else {
        print_text(&cfg, &rep);
    }
    Ok(exit::OK)
}

async fn collect(cfg: &ClientConfig) -> Report {
    let root = proj::root();
    let root_str = root.to_string_lossy().to_string();
    let memory_dir = proj::memory_dir(&root);

    let mut rep = Report {
        project: root_str.clone(),
        project_key: project::key(&proj::remote(), &root_str),
        memory_dir: memory_dir.display().to_string(),
        memory_files: state::list_memory_files(&memory_dir)
            .map(|f| f.len())
            .unwrap_or(0),
        hooks_wired: std::fs::read(root.join(".claude").join("settings.json"))
            .map(|b| settings::is_wired(&b))
            .unwrap_or(false),
        global_key: cfg.global_key.clone(),
        global_files: state::list_memory_files(&memory_dir.join(scope::GLOBAL_DIR))
            .map(|f| f.len())
            .unwrap_or(0),
        global_linked: std::fs::read(memory_dir.join("MEMORY.md"))
            .map(|b| recall_hooks::global_index_is_linked(&b))
            .unwrap_or(false),
        url_set: !cfg.url.is_empty(),
        token_set: !cfg.token.is_empty(),
        server_ok: false,
        server_error: None,
        git_commit: None,
        merge_ready: false,
        synced_files: 0,
        last_synced_at: None,
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

fn print_text(cfg: &ClientConfig, rep: &Report) {
    println!("project      : {}", rep.project);
    println!("project_key  : {}", rep.project_key);
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
    println!(
        "global       : {}",
        match &rep.global_key {
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
        "RECALL_URL   : {}",
        if cfg.url.is_empty() {
            "(unset)"
        } else {
            &cfg.url
        }
    );
    println!(
        "RECALL_TOKEN : {}",
        if cfg.token.is_empty() {
            "(unset)"
        } else {
            "set"
        }
    );

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
