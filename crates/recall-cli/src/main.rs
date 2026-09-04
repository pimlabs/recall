//! `recall` — sync Claude Code's auto memory across machines.
//!
//! One binary, both halves: `recall serve` runs the server, everything else
//! runs on a developer machine or inside a Claude Code session as a hook.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{Parser, Subcommand};
use recall_hooks::hookio::{self, EXIT_CONFIG, EXIT_OK, EXIT_SERVER};
use recall_hooks::{hooks, settings, state, syncclient};
use recall_paths::{config, key, Env as ClaudeEnv};

/// Set at build time by the release workflow.
const VERSION: &str = env!("CARGO_PKG_VERSION");
const COMMIT: Option<&str> = option_env!("RECALL_GIT_COMMIT");

#[derive(Parser)]
#[command(
    name = "recall",
    about = "Sync Claude Code's auto memory across machines and cloud sessions",
    disable_version_flag = true
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Wire the current project's .claude/settings.json for sync
    Init {
        /// Project to wire; defaults to the git root of the working directory
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Show whether sync is configured and reachable, here
    Status {
        /// Machine-readable output, for scripts and CI
        #[arg(long)]
        json: bool,
    },
    /// Run the sync server
    Serve,
    /// Hook entry point — called by PostToolUse
    Push,
    /// Hook entry point — called by SessionStart
    Pull,
    /// Print the version
    Version,
}

fn main() {
    let cli = Cli::parse();

    // Only `serve` is long-running and genuinely concurrent. The hook
    // commands each make one request and exit, so they get a
    // single-threaded runtime — `recall push` runs on every memory write in
    // a session, and spinning up a thread pool to make one HTTP call is
    // waste the user pays for repeatedly.
    let result = match cli.command {
        Cmd::Version => {
            println!("recall {VERSION} ({})", COMMIT.unwrap_or("unknown"));
            Ok(EXIT_OK)
        }
        Cmd::Init { path } => cmd_init(path.as_deref()),
        Cmd::Serve => block_on_multi(cmd_serve()),
        Cmd::Status { json } => block_on_current(cmd_status(json)),
        Cmd::Push => block_on_current(cmd_push()),
        Cmd::Pull => block_on_current(cmd_pull()),
    };

    match result {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("recall: {err:#}");
            std::process::exit(EXIT_CONFIG)
        }
    }
}

fn block_on_current<F: std::future::Future<Output = anyhow::Result<i32>>>(
    fut: F,
) -> anyhow::Result<i32> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(fut)
}

fn block_on_multi<F: std::future::Future<Output = anyhow::Result<i32>>>(
    fut: F,
) -> anyhow::Result<i32> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(fut)
}

/// The project root, resolved the way Claude Code resolves it: the git root,
/// falling back to the working directory.
fn project_root() -> PathBuf {
    if let Some(root) = git(&["rev-parse", "--show-toplevel"]) {
        return PathBuf::from(root);
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn git_remote() -> String {
    git(&["remote", "get-url", "origin"]).unwrap_or_default()
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// Everything the hook commands need, assembled from the environment.
fn hook_env() -> anyhow::Result<hooks::Env> {
    let cfg = config::Client::from_process_env();
    cfg.require()?;

    let root = project_root();
    let root_str = root.to_string_lossy().to_string();
    let claude = ClaudeEnv::from_process_env();

    Ok(hooks::Env {
        memory_dir: claude.memory_dir(&root_str),
        state_file: claude.state_file(&root_str),
        project_key: key(&git_remote(), &root_str),
        source_env: cfg.source_env.clone(),
        client: syncclient::Client::new(&cfg.url, &cfg.token)?,
    })
}

fn cmd_init(path: Option<&Path>) -> anyhow::Result<i32> {
    let root = match path {
        Some(p) => p.to_path_buf(),
        None => match git(&["rev-parse", "--show-toplevel"]) {
            Some(root) => PathBuf::from(root),
            None => anyhow::bail!(
                "not inside a git repository — run this from the project you want to sync"
            ),
        },
    };
    let settings_path = root.join(".claude").join("settings.json");

    if settings::wire_file(&settings_path)? {
        println!("recall: wired hooks into {}", settings_path.display());
        println!(
            "
  Next: review and commit the change, so fresh clones and cloud sessions
  pick it up too — that's what makes this work without per-machine setup.

    git -C {0} diff .claude/settings.json
    git -C {0} add .claude/settings.json && git -C {0} commit -m \"Enable Recall memory sync\"",
            root.display()
        );
    } else {
        println!(
            "recall: already wired, nothing to change ({})",
            settings_path.display()
        );
    }

    let cfg = config::Client::from_process_env();
    if cfg.url.is_empty() || cfg.token.is_empty() {
        println!();
        if cfg.url.is_empty() {
            println!("  ! RECALL_URL is not set in this shell");
        }
        if cfg.token.is_empty() {
            println!("  ! RECALL_TOKEN is not set in this shell");
        }
        println!(
            "
  Add these to your shell profile (~/.zshrc, ~/.bashrc) before sync works:

    export RECALL_URL=\"https://your-recall-host\"
    export RECALL_TOKEN=\"<your token>\"

  See docs/token-setup.md for generating the token, and for the extra
  variables a claude.ai cloud environment needs."
        );
    }
    Ok(EXIT_OK)
}

#[derive(serde::Serialize)]
struct StatusReport {
    project: String,
    project_key: String,
    memory_dir: String,
    memory_files: usize,
    hooks_wired: bool,
    url_set: bool,
    token_set: bool,
    server_ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    server_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<String>,
    merge_ready: bool,
    synced_files: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_synced_at: Option<String>,
}

async fn cmd_status(as_json: bool) -> anyhow::Result<i32> {
    let cfg = config::Client::from_process_env();
    let root = project_root();
    let root_str = root.to_string_lossy().to_string();
    let claude = ClaudeEnv::from_process_env();
    let memory_dir = claude.memory_dir(&root_str);

    let mut rep = StatusReport {
        project: root_str.clone(),
        project_key: key(&git_remote(), &root_str),
        memory_dir: memory_dir.display().to_string(),
        memory_files: state::list_memory_files(&memory_dir)
            .map(|f| f.len())
            .unwrap_or(0),
        hooks_wired: std::fs::read(root.join(".claude").join("settings.json"))
            .map(|b| settings::is_wired(&b))
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

    if rep.url_set {
        match syncclient::Client::new(&cfg.url, &cfg.token) {
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
    }

    if as_json {
        println!("{}", serde_json::to_string_pretty(&rep)?);
        return Ok(EXIT_OK);
    }

    println!("project      : {}", rep.project);
    println!("project_key  : {}", rep.project_key);
    println!("memory dir   : {}", rep.memory_dir);
    println!("memory files : {} on disk", rep.memory_files);
    println!(
        "hooks wired  : {}",
        if rep.hooks_wired {
            "yes".to_string()
        } else {
            "NO — run 'recall init' in this project".to_string()
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
    if rep.url_set {
        if rep.server_ok {
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
        } else {
            println!(
                "server       : UNREACHABLE ({})",
                rep.server_error.as_deref().unwrap_or("unknown error")
            );
        }
    }
    Ok(EXIT_OK)
}

async fn cmd_push() -> anyhow::Result<i32> {
    let payload = hookio::parse_post_tool_use(io::stdin().lock());
    if payload.tool_input.file_path.is_empty() {
        return Ok(EXIT_OK);
    }
    let triggered = PathBuf::from(&payload.tool_input.file_path);

    // Decide whether this is even our business before asking for
    // configuration. This hook fires on every Edit and Write in the session,
    // and a machine that has cloned a wired project without being configured
    // yet — the exact case Recall exists for — would otherwise report a
    // missing token on every unrelated file the user touches.
    let root = project_root();
    let memory_dir = ClaudeEnv::from_process_env().memory_dir(&root.to_string_lossy());
    if !hooks::is_memory_file(&memory_dir, &triggered) {
        return Ok(EXIT_OK);
    }

    let env = hook_env()?;
    match hooks::push(&env, &triggered).await {
        Ok(res) => {
            if res.pushed.is_some() || !res.deleted.is_empty() {
                eprintln!(
                    "recall-push: pushed {}, deleted {} for {}",
                    usize::from(res.pushed.is_some()),
                    res.deleted.len(),
                    env.project_key
                );
            }
            Ok(EXIT_OK)
        }
        Err(err) => {
            eprintln!("recall-push: {err}");
            Ok(EXIT_SERVER)
        }
    }
}

async fn cmd_pull() -> anyhow::Result<i32> {
    // A pull must never be the reason a session can't start: an unconfigured
    // or unreachable server warns on stderr and exits 0, leaving whatever is
    // already on disk alone.
    let env = match hook_env() {
        Ok(env) => env,
        Err(err) => {
            eprintln!("recall-pull: {err}, leaving local memory untouched");
            return Ok(EXIT_OK);
        }
    };
    match hooks::pull(&env).await {
        Ok(res) => {
            eprintln!("{}", res.describe(&env.project_key));
            Ok(EXIT_OK)
        }
        Err(err) => {
            eprintln!("recall-pull: fetch failed ({err}), leaving local memory untouched");
            Ok(EXIT_OK)
        }
    }
}

async fn cmd_serve() -> anyhow::Result<i32> {
    let cfg = recall_server::Config::from_env()?;
    // Opening the store before constructing the server means a bad database
    // path fails immediately with a clear error, rather than after the port
    // is already bound.
    let store = std::sync::Arc::new(recall_server::Store::open(&cfg.db_path)?);
    recall_server::Server::new(cfg, store).serve().await?;
    Ok(EXIT_OK)
}
