//! `recall status` — the command people run when something is wrong, so it
//! has to work when everything is wrong.
//!
//! Nothing here is allowed to fail the process: an unset variable, a dead
//! server and a project that was never wired are all *findings*, reported in
//! the output, not errors.

use recall_hooks::config::Source;
use recall_hooks::declared_env::{Declared, Ignored};
use recall_hooks::{claude, config, exit, project, scope, settings, state, ClientConfig};

use crate::project as proj;
use crate::ui::{self, Tone};

/// Every variable Recall reads, in the order status reports them.
///
/// Assembled from the two crates that own the reads rather than retyped, so
/// a variable added there cannot be silently missing here — which would
/// reintroduce, one variable at a time, exactly the blind spot this report
/// was fixed to remove.
pub(crate) fn known_vars() -> Vec<&'static str> {
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

/// This machine's device at the server in effect, as `status --json`
/// reports it. Never the key itself.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DeviceReport {
    /// `dev_…`.
    pub id: String,
    /// The name the server knows it by, which its pushes are stored under.
    pub name: String,
    /// `sync` or `admin`.
    pub scope: String,
    /// Whether the server removes it once idle.
    pub ephemeral: bool,
    /// Where the private key is kept: always `file` today, see
    /// `recall_hooks::home`.
    pub key_storage: &'static str,
    /// The file.
    pub key_file: String,
    /// Whether the server confirmed it with `GET /v1/devices/me`: absent
    /// when it was not asked, because it did not answer at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmed: Option<bool>,
    /// Whether the server refused it as unknown or revoked, which only
    /// enrolling again mends.
    pub gone: bool,
    /// What the server said when it did not confirm it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check_error: Option<String>,
}

/// This machine's witness of the server's audit log, as `status --json`
/// reports it: the checkpoints saved in `~/.recall/audit.json`, and what
/// checking them found. See `recall_hooks::audit`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct AuditReport {
    /// `~/.recall/audit.json`.
    pub file: String,
    /// Checkpoints a proof has shown the log extends.
    pub checkpoints: usize,
    /// Checkpoints saved by pulls and not yet proven.
    pub unchecked: usize,
    /// The newest proven one, as `<tree_size> <root_hash>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest: Option<String>,
    /// Whether the server keeps an audit log, per its discovery document:
    /// absent when it was not asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_log: Option<bool>,
    /// Whether the server just proved its log extends every checkpoint
    /// saved here: absent when it was not asked, `false` once an
    /// inconsistency is found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extends: Option<bool>,
    /// The server answered, and not with a proof: what it said. Not a
    /// finding about its log, and not written down, but no proof either.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unproven: Option<String>,
    /// Why the check did not finish: the server did not answer (rate
    /// limited, unreachable, too slow), or what it proved could not be
    /// written down. Nothing about its log follows from this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Why `audit.json` itself could not be read. It may hold the only
    /// record of a rewrite, so this is a failure, not a detail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_error: Option<String>,
    /// When the oldest checkpoint still unchecked was saved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unchecked_since: Option<String>,
    /// When a check last finished, proving every checkpoint saved then.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_proven_at: Option<String>,
    /// Whether the server refused this machine's credential for the audit
    /// routes (401, 403): [`AuditReport::error`] says what it answered.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub refused: bool,
    /// How many unchecked checkpoints were dropped past the bound since the
    /// last reset: a gap in the witnessing.
    #[serde(skip_serializing_if = "is_zero")]
    pub dropped: u64,
    /// How many checks in a row the server left unanswered.
    #[serde(skip_serializing_if = "is_zero")]
    pub unanswered: u64,
    /// What checking found once the log did not extend a checkpoint saved
    /// here. Kept until `recall audit reset`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inconsistent: Option<recall_hooks::audit::Inconsistency>,
    /// Why that finding could not be written into `audit.json` this time,
    /// when it could not: it stands all the same.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsaved: Option<String>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl AuditReport {
    /// How many checkpoints are saved, checked or not.
    pub fn saved(&self) -> usize {
        self.checkpoints + self.unchecked
    }

    /// Reads what `witness` holds into the report, keeping what was found
    /// by asking; a file that cannot be read is said so, and never taken
    /// for an empty one.
    fn read(&mut self, witness: &recall_hooks::audit::Witness) {
        match witness.load() {
            Ok(saved) => {
                self.checkpoints = saved.checkpoints.len();
                self.unchecked = saved.unchecked.len();
                self.newest = saved.newest().map(|c| c.header());
                self.unchecked_since = saved.unchecked_since;
                self.last_proven_at = saved.last_proven_at;
                self.dropped = saved.dropped;
                self.unanswered = saved.unanswered;
                if saved.inconsistent.is_some() {
                    self.inconsistent = saved.inconsistent;
                }
            }
            Err(e) => self.file_error = Some(e.to_string()),
        }
    }
}

/// How long `recall status` and `recall doctor` wait on the server.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Deadlines {
    /// For everything but the audit check, in all: every request has a
    /// timeout of its own, and a slow server answering each just inside it
    /// would otherwise hold the command for minutes.
    pub server: std::time::Duration,
    /// For the audit check, its rate-limited retries included, on top of
    /// that. A budget of its own, so that a server slow at everything else
    /// cannot keep the check from ever being asked, and pending for ever:
    /// what the check proves before its deadline is kept, what it does not
    /// is counted as unanswered, and `recall doctor` fails on either kind
    /// of stall in the end.
    pub audit: std::time::Duration,
}

/// The deadlines both commands run with.
const DEADLINES: Deadlines = Deadlines {
    server: std::time::Duration::from_secs(90),
    audit: std::time::Duration::from_secs(20),
};

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
    /// Which credential this machine's requests carry: `device` when it
    /// signs them with its device key, `bearer` when it sends
    /// `RECALL_TOKEN`, `none` when it has neither.
    pub auth: &'static str,
    /// This machine's device at the server in effect, when it is enrolled
    /// there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<DeviceReport>,
    /// `~/.recall/device.key`, when there is a `~/.recall` to look in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_file: Option<String>,
    /// Why the device key file could not be used, when it exists and could
    /// not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_error: Option<String>,
    /// Whether anyone but its owner can read the device key file.
    pub device_file_exposed: bool,
    /// Whether `RECALL_AUTHKEY` is set, with which a session enrols
    /// itself at its first pull.
    pub authkey_set: bool,
    /// Whether the server enrols devices and accepts their signatures, per
    /// its discovery document. Absent when it did not say: unreachable, or
    /// older than the document.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_devices: Option<bool>,
    /// Whether `GET /health` answered.
    pub server_ok: bool,
    /// Why it didn't, when it didn't.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_error: Option<String>,
    /// The commit the server was built from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<String>,
    /// The server's version, from `GET /.well-known/recall`. [`None`] from a
    /// server older than that document.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_version: Option<String>,
    /// `release` or `dev`: whether the server is a release build.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_channel: Option<String>,
    /// The protocol versions the server speaks. Empty when it did not say.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub server_protocols: Vec<u32>,
    /// The oldest client version the server accepts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_client: Option<String>,
    /// This client's version, as it reports itself to the server.
    pub client_version: String,
    /// Whether the server can actually merge, or is silently falling back to
    /// last-write-wins. With a merge worker, whether the worker's CLI can.
    pub merge_ready: bool,
    /// Whether a merge worker is enrolled, so merges run there, from a
    /// queue, rather than inside the server.
    pub merge_worker: bool,
    /// The worker's queue, when there is a worker, or when it holds
    /// anything (a queue a revoked worker left, or failed merges).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_queue: Option<recall_wire::QueueStatus>,
    /// When the merge worker was last heard from: its last request for
    /// work, or the server's start when it has made none since. [`None`]
    /// without a worker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_worker_seen_at: Option<String>,
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
    /// This machine's witness of the server's audit log, when there is a
    /// server and a `~/.recall` to keep checkpoints in. Read from the file
    /// whether or not the server answers, so a rewrite found earlier is
    /// reported even while it is down.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audit: Option<AuditReport>,
    /// `GET /health` as the server answered it, for `recall review` to
    /// read what this report summarises (the last backup, which it does
    /// not) without asking the server twice. Never in the JSON: the fields
    /// above are the contract.
    #[serde(skip)]
    pub health: Option<recall_wire::Health>,
    /// The discovery document as the server answered it, kept for the
    /// same reason and never in the JSON either.
    #[serde(skip)]
    pub discovery: Option<recall_wire::discovery::Discovery>,
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
    collect_within(here, cfg, DEADLINES).await
}

/// [`collect`], waiting on the server as long as `deadlines` says.
pub(crate) async fn collect_within(
    here: &proj::Resolved,
    cfg: &ClientConfig,
    deadlines: Deadlines,
) -> Report {
    collect_asking(here, cfg, deadlines, Asks::Everything).await
}

/// The report `recall review` reads: everything this machine knows about
/// itself, and what the server says it is (`/health` and the discovery
/// document), waiting on the server at most `server`. None of what
/// `recall status` also asks to check this machine's own standing: no
/// pull, no enrolment check, no audit witness check. A review is not the
/// place to learn those, and the witness check in particular counts an
/// unanswered check against the server, which a review has no business
/// doing.
pub(crate) async fn collect_for_review(
    here: &proj::Resolved,
    cfg: &ClientConfig,
    server: std::time::Duration,
) -> Report {
    let deadlines = Deadlines {
        server,
        audit: std::time::Duration::ZERO,
    };
    collect_asking(here, cfg, deadlines, Asks::WhatTheServerIs).await
}

/// How much of the server [`collect_asking`] asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asks {
    /// Everything `recall status` and `recall doctor` report.
    Everything,
    /// `/health` and the discovery document only.
    WhatTheServerIs,
}

async fn collect_asking(
    here: &proj::Resolved,
    cfg: &ClientConfig,
    deadlines: Deadlines,
    asks: Asks,
) -> Report {
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
        // Nothing is sent while `device.key` cannot be read, whatever else
        // there is: see `ClientConfig::client`.
        auth: if cfg.device_error.is_some() {
            "none"
        } else if cfg.device.is_some() {
            "device"
        } else if !cfg.token.is_empty() {
            "bearer"
        } else {
            "none"
        },
        device: cfg.device.as_ref().map(|d| DeviceReport {
            id: d.device_id.clone(),
            name: d.name.clone(),
            scope: d.scope.clone(),
            ephemeral: d.ephemeral,
            key_storage: "file",
            key_file: cfg
                .device_file
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            confirmed: None,
            gone: false,
            check_error: None,
        }),
        device_file: cfg.device_file.as_ref().map(|p| p.display().to_string()),
        device_error: cfg.device_error.clone(),
        device_file_exposed: cfg
            .device_file
            .as_deref()
            .is_some_and(recall_hooks::home::readable_by_others),
        authkey_set: cfg.authkey.is_some(),
        server_devices: None,
        machine_source: cfg.machine_source,
        config_problems: cfg.config_problems.clone(),
        overridden: overrides(here, cfg),
        server_ok: false,
        server_error: None,
        git_commit: None,
        server_version: None,
        server_channel: None,
        server_protocols: Vec::new(),
        min_client: None,
        client_version: recall_wire::discovery::version(),
        merge_ready: false,
        merge_worker: false,
        merge_queue: None,
        merge_worker_seen_at: None,
        synced_files: 0,
        last_synced_at: None,
        last_offbox_at: None,
        audit: None,
        health: None,
        discovery: None,
    };

    if !rep.url_set {
        return rep;
    }
    let witness = cfg
        .audit_file
        .as_ref()
        .filter(|_| asks == Asks::Everything)
        .map(|file| recall_hooks::audit::Witness::new(file, &cfg.url));
    if let Some(witness) = &witness {
        let mut audit = AuditReport {
            file: witness.file().display().to_string(),
            ..Default::default()
        };
        audit.read(witness);
        rep.audit = Some(audit);
    }

    // A device key that cannot be used is the device's problem, reported as
    // `device_error`, and says nothing about whether the server is up.
    // `/health` and the discovery document are asked anyway, with no
    // credential at all: neither needs one, and this machine has none it
    // may send.
    let (client, usable) = match cfg.client() {
        Ok(client) => (Ok(client), true),
        Err(e) if e.is_device() => {
            rep.device_error.get_or_insert_with(|| e.to_string());
            rep.auth = "none";
            let anonymous = recall_hooks::client::Client::new(&cfg.url, "");
            (anonymous.map_err(|e| e.to_string()), false)
        }
        Err(e) => (Err(e.to_string()), false),
    };
    match client {
        Ok(client) => {
            let asked = ask_server(&mut rep, &client, usable, asks);
            let finished = tokio::time::timeout(deadlines.server, asked).await.is_ok();
            if !finished && !rep.server_ok {
                rep.server_error.get_or_insert(format!(
                    "no answer within {} seconds in all",
                    deadlines.server.as_secs()
                ));
            }
            // After the pull, which saved the checkpoint it carried: the
            // check proves that one too. What `recall doctor` is for, per
            // docs/design/part5-plan.md's "Who witnesses". Outside the
            // deadline above, on one of its own, and asked however the
            // rest went: a server slow at everything else would otherwise
            // never be asked at all. Asked of any server not known to keep
            // no log: discovery failing is no reason to leave saved
            // checkpoints unproven, and the audit routes answer for
            // themselves (404 is no log).
            let credential = usable && (rep.token_set || rep.device.is_some());
            if let (Some(witness), Some(audit)) = (&witness, rep.audit.as_mut()) {
                audit.read(witness);
                if credential && audit.server_log != Some(false) && audit.file_error.is_none() {
                    witness_check(witness, &client, audit, deadlines.audit).await;
                }
            }
        }
        Err(err) => rep.server_error = Some(err),
    }
    rep
}

/// Everything `collect` asks the server but the audit check, in order,
/// filling in `rep`: what is filled in before the server's deadline passes
/// stays, whatever is not reached.
async fn ask_server(
    rep: &mut Report,
    client: &recall_hooks::client::Client,
    usable: bool,
    asks: Asks,
) {
    match client.health().await {
        Ok(health) => {
            rep.health = Some(health.clone());
            rep.server_ok = true;
            rep.git_commit = Some(health.git_commit);
            rep.merge_ready = health.merge.claude_cli.logged_in.unwrap_or(false);
            rep.merge_worker = health.merge.worker.is_some();
            rep.merge_queue = health.merge.queue;
            rep.merge_worker_seen_at = health
                .merge
                .worker
                .map(|w| w.last_claim_at.unwrap_or_else(|| health.started_at.clone()));
            if !health.last_sync_at.is_empty() {
                rep.last_synced_at = Some(health.last_sync_at);
            }
            if !health.last_offbox_at.is_empty() {
                rep.last_offbox_at = Some(health.last_offbox_at);
            }
        }
        Err(err) => rep.server_error = Some(err.to_string()),
    }
    // Asked only of a server that answered: an unreachable one has
    // already been reported, and a second error would say nothing new.
    if rep.server_ok {
        let doc = client.discover().await;
        if let Some(audit) = rep.audit.as_mut() {
            // A server older than the discovery document is older
            // than the audit log too.
            audit.server_log = match &doc {
                Ok(Some(doc)) => Some(doc.audit().is_some()),
                Ok(None) => Some(false),
                Err(_) => None,
            };
        }
        if let Ok(Some(doc)) = doc {
            rep.discovery = Some(doc.clone());
            rep.server_devices = Some(
                doc.accepts(recall_wire::discovery::AUTH_DEVICE_SIG) && doc.devices().is_some(),
            );
            rep.server_version = Some(doc.server.version);
            rep.server_channel = Some(doc.server.build.channel);
            rep.server_protocols = doc.protocol.supported;
            rep.min_client = Some(doc.min_client);
        }
    }
    if asks == Asks::WhatTheServerIs {
        return;
    }
    // The one request that says whether this machine is still
    // enrolled: a device key the server has revoked or swept looks
    // exactly like a working one from here.
    if rep.server_ok && usable {
        if let Some(device) = rep.device.as_mut() {
            match client.me().await {
                Ok(me) => {
                    device.confirmed = Some(true);
                    device.name = me.name;
                    device.scope = me.scope;
                    device.ephemeral = me.ephemeral;
                }
                Err(e) => {
                    device.confirmed = Some(false);
                    device.gone = e.device_gone();
                    device.check_error = Some(e.reason());
                }
            }
        }
    }
    if usable && (rep.token_set || rep.device.is_some()) {
        if let Ok(resp) = client.pull(&rep.project_key).await {
            rep.synced_files = resp.files.iter().filter(|f| !f.deleted).count();
        }
    }
}

/// Asks the server to prove its log extends every checkpoint saved here,
/// within `deadline`, and reads what that found into `audit`.
async fn witness_check(
    witness: &recall_hooks::audit::Witness,
    client: &recall_hooks::client::Client,
    audit: &mut AuditReport,
    deadline: std::time::Duration,
) {
    use recall_hooks::audit::{CheckError, Witnessed};
    let mut found = None;
    match witness.check(client, deadline).await {
        Ok(Witnessed::Extends { .. }) => audit.extends = Some(true),
        Ok(Witnessed::Inconsistent { finding, unsaved }) => {
            audit.extends = Some(false);
            audit.unsaved = unsaved;
            found = Some(finding);
        }
        Err(e) if e.unreadable() => audit.file_error = Some(e.to_string()),
        Err(e) if e.no_log() => audit.server_log = Some(false),
        Err(e) if e.refused() => {
            audit.refused = true;
            audit.error = Some(e.to_string());
        }
        Err(e @ (CheckError::File(_) | CheckError::Deadline(_) | CheckError::Moved)) => {
            audit.error = Some(e.to_string())
        }
        Err(e) if e.unanswered() => audit.error = Some(e.to_string()),
        Err(e) => audit.unproven = Some(e.to_string()),
    }
    audit.read(witness);
    // A finding the file could not take is still the finding.
    if audit.inconsistent.is_none() {
        audit.inconsistent = found;
    }
}

/// Variables in the environment that override a different value in
/// `config.toml`.
///
/// Compared as each value is *used*, not as it was typed: `jarvis` and
/// `machine:jarvis` are the same machine key, and `https://x/` is the same
/// server as `https://x`, so neither is reported as an override.
pub(crate) fn overrides(here: &proj::Resolved, cfg: &ClientConfig) -> Vec<Override> {
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

/// Where a variable in the environment was set, as far as it can be known.
///
/// A settings file can be named, because it was read. The shell cannot: by
/// the time a process sees a variable, which profile exported it is gone.
fn origin(rep: &Report, name: &str) -> String {
    match declared_in(rep, name) {
        Some(file) => format!("set by {file}"),
        None if rep.remote_session => "set in the environment".to_string(),
        None => "set in this shell".to_string(),
    }
}

/// What connects this machine, for a line reporting that nothing does.
fn connect_hint(rep: &Report, url: &str) -> String {
    match (rep.remote_session, url.is_empty()) {
        (true, _) => "set RECALL_URL and RECALL_AUTHKEY on the cloud environment".to_string(),
        (false, true) => "`recall connect https://your-recall-host`".to_string(),
        (false, false) => format!("`recall connect {url}`"),
    }
}

/// `1 file`, `3 files`.
fn files(n: usize) -> String {
    match n {
        1 => "1 file".to_string(),
        n => format!("{n} files"),
    }
}

/// How many characters of a hash or a commit a person is shown: plenty to
/// tell two apart. `--json` carries the whole value.
const SHORT: usize = 12;

/// A checkpoint as a person reads it, `<size> <root>` with the root cut to
/// [`SHORT`] characters.
pub(crate) fn short_checkpoint(header: &str) -> String {
    match header.split_once(' ') {
        Some((size, root)) if root.chars().count() > SHORT => {
            format!("{size} {}…", root.chars().take(SHORT).collect::<String>())
        }
        _ => header.to_string(),
    }
}

/// A commit cut to [`SHORT`] characters, as git abbreviates one.
pub(crate) fn short_commit(commit: &str) -> String {
    commit.chars().take(SHORT).collect()
}

/// What a report is about, after the command's name: this machine's name,
/// and the server it talks to. Shared with `recall doctor`, which opens the
/// same way.
pub(crate) fn about(cfg: &ClientConfig) -> String {
    let server = cfg
        .url
        .split_once("://")
        .map(|(_, rest)| rest.trim_end_matches('/'))
        .unwrap_or("no server");
    format!("{} → {server}", cfg.source_env)
}

/// The groups the text report is read in, in `recall doctor`'s order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    Connection,
    Project,
    Scopes,
    History,
}

impl Group {
    const ALL: [Group; 4] = [
        Group::Connection,
        Group::Project,
        Group::Scopes,
        Group::History,
    ];

    fn name(self) -> &'static str {
        match self {
            Group::Connection => "Connection",
            Group::Project => "This project",
            Group::Scopes => "Scopes",
            Group::History => "History",
        }
    }
}

/// One line of the text report: its mark, what it is about, what was found
/// and, when something is wrong, what to run about it.
///
/// Built only through the four constructors below, which is what makes the
/// rule `recall doctor` keeps hold here too: a problem cannot be written
/// down without what to do about it, because a problem nobody can act on
/// teaches the reader to skip the report.
#[derive(Debug)]
struct Line {
    group: Group,
    tone: Tone,
    label: &'static str,
    detail: String,
    fix: Option<String>,
    /// A dimmed line under the detail: a long path, say, which would push
    /// the part worth reading off the end of the line.
    under: Option<String>,
}

impl Line {
    fn new(group: Group, tone: Tone, label: &'static str, detail: String) -> Line {
        Line {
            group,
            tone,
            label,
            detail,
            fix: None,
            under: None,
        }
    }

    /// Working.
    fn good(group: Group, label: &'static str, detail: impl Into<String>) -> Line {
        Line::new(group, Tone::Good, label, detail.into())
    }

    /// Off by choice, or not applicable here.
    fn quiet(group: Group, label: &'static str, detail: impl Into<String>) -> Line {
        Line::new(group, Tone::Quiet, label, detail.into())
    }

    /// Worth a look, and what to run about it.
    fn warn(
        group: Group,
        label: &'static str,
        detail: impl Into<String>,
        fix: impl Into<String>,
    ) -> Line {
        Line {
            fix: Some(fix.into()),
            ..Line::new(group, Tone::Warn, label, detail.into())
        }
    }

    /// Broken, and what to run about it.
    fn bad(
        group: Group,
        label: &'static str,
        detail: impl Into<String>,
        fix: impl Into<String>,
    ) -> Line {
        Line {
            fix: Some(fix.into()),
            ..Line::new(group, Tone::Bad, label, detail.into())
        }
    }

    fn under(mut self, text: impl Into<String>) -> Line {
        self.under = Some(text.into());
        self
    }
}

/// Reads the report into lines, most fundamental first within each group.
fn lines(cfg: &ClientConfig, rep: &Report) -> Vec<Line> {
    let mut out = Vec::new();
    connection_lines(cfg, rep, &mut out);
    project_lines(rep, &mut out);
    scope_lines(rep, &mut out);
    if let Some(audit) = &rep.audit {
        out.push(audit_line(audit, rep.server_ok));
    }
    out
}

fn connection_lines(cfg: &ClientConfig, rep: &Report, out: &mut Vec<Line>) {
    use Group::Connection as C;
    let config_file = rep.config_file.as_deref().unwrap_or("the config file");
    let credentials_file = rep
        .credentials_file
        .as_deref()
        .unwrap_or("the credentials file");

    out.push(match rep.url_source {
        Source::Unset => Line::bad(
            C,
            "RECALL_URL",
            "not set, so nothing syncs",
            connect_hint(rep, ""),
        ),
        Source::Environment => Line::good(
            C,
            "RECALL_URL",
            format!("{}, {}", cfg.url, origin(rep, "RECALL_URL")),
        ),
        Source::CredentialsFile | Source::ConfigFile => Line::good(
            C,
            "RECALL_URL",
            format!("{}, saved in {config_file}", cfg.url),
        ),
    });
    out.push(match rep.token_source {
        Source::Unset if rep.device.is_some() => Line::quiet(
            C,
            "RECALL_TOKEN",
            "not needed, this machine signs its requests",
        ),
        Source::Unset if rep.authkey_set => Line::quiet(
            C,
            "RECALL_TOKEN",
            "not needed, RECALL_AUTHKEY enrols this session as a device",
        ),
        Source::Unset => Line::bad(C, "RECALL_TOKEN", "not set", connect_hint(rep, &cfg.url)),
        Source::Environment => Line::good(C, "RECALL_TOKEN", origin(rep, "RECALL_TOKEN")),
        Source::CredentialsFile | Source::ConfigFile => {
            Line::good(C, "RECALL_TOKEN", format!("saved in {credentials_file}"))
        }
    });

    if rep.url_set && !rep.server_ok {
        let fix = if rep.remote_session {
            "add the server's domain under the cloud environment's Allowed domains".to_string()
        } else {
            format!(
                "See what answers there: `curl -sS {}/health`",
                cfg.url.trim_end_matches('/')
            )
        };
        out.push(Line::bad(
            C,
            "server",
            format!(
                "unreachable: {}",
                rep.server_error.as_deref().unwrap_or("unknown error")
            ),
            fix,
        ));
    }
    if rep.server_ok {
        out.push(Line::good(
            C,
            "server",
            match (&rep.server_version, rep.server_channel.as_deref()) {
                (Some(v), Some("release") | None) => format!("answered, {v}"),
                (Some(v), Some(channel)) => format!("answered, {v} ({channel} build)"),
                (None, _) => format!(
                    "answered, commit {}",
                    short_commit(rep.git_commit.as_deref().unwrap_or("unknown"))
                ),
            },
        ));
        let parse = recall_wire::discovery::Version::parse;
        let too_old = rep.min_client.as_deref().filter(|min| {
            matches!(
                (parse(&rep.client_version), parse(min)),
                (Some(mine), Some(min)) if mine < min
            )
        });
        out.push(match too_old {
            Some(min) => Line::bad(
                C,
                "client",
                format!(
                    "{}, and the server needs at least {min}",
                    rep.client_version
                ),
                "With Homebrew: `brew upgrade recall` Or with npm: \
                 `npm install -g @pimlabs/recall`",
            ),
            None => Line::good(C, "client", rep.client_version.clone()),
        });
    }

    device_lines(rep, out);

    if let Some(err) = &rep.credentials_error {
        out.push(Line::warn(
            C,
            "credentials",
            format!("{err}, so nothing in it is in effect"),
            format!("Move {credentials_file} aside, then connect again: `recall connect`"),
        ));
    }
    if rep.credentials_exposed {
        out.push(Line::warn(
            C,
            "credentials",
            format!("{credentials_file} is readable by other users"),
            format!("`chmod 600 {credentials_file}`"),
        ));
    }
    for problem in &rep.config_problems {
        out.push(Line::warn(
            C,
            "config",
            format!("{problem}, in {config_file}"),
            format!("edit {config_file}"),
        ));
    }
    for o in &rep.overridden {
        let from = declared_in(rep, o.variable).unwrap_or("your shell profile");
        out.push(Line::warn(
            C,
            "config",
            format!(
                "{}={} overrides {} = {:?} in {config_file}",
                o.variable, o.environment, o.setting, o.config
            ),
            format!(
                "remove {} from {from}, or change {} to match",
                o.variable, o.setting
            ),
        ));
    }

    if rep.server_ok {
        merge_lines(rep, out);
    }
}

/// This machine's device, when it has one or the server would enrol one.
fn device_lines(rep: &Report, out: &mut Vec<Line>) {
    use Group::Connection as C;
    let key_file = rep.device_file.as_deref().unwrap_or("~/.recall/device.key");
    if let Some(err) = &rep.device_error {
        out.push(Line::bad(
            C,
            "device",
            format!("{err}, so this machine sends nothing to the server"),
            format!("Move {key_file} aside, then enrol again: `recall connect`"),
        ));
    }
    if rep.device_file_exposed {
        out.push(Line::warn(
            C,
            "device",
            format!("{key_file} is readable by other users"),
            format!("`chmod 600 {key_file}`"),
        ));
    }
    let Some(d) = &rep.device else {
        if rep.authkey_set {
            out.push(Line::quiet(
                C,
                "device",
                "none yet, RECALL_AUTHKEY enrols one at the next pull",
            ));
        } else if rep.server_devices == Some(true) && rep.token_set {
            out.push(Line::warn(
                C,
                "device",
                "not enrolled, so this machine uses the shared RECALL_TOKEN",
                if rep.remote_session {
                    "On an admin device, make an auth key: \
                     `recall authkey create --tag cloud --expires 90d` Then set RECALL_AUTHKEY \
                     on the cloud environment, and remove RECALL_TOKEN."
                } else {
                    "`recall connect`"
                },
            ));
        }
        return;
    };
    let what = format!(
        "{} ({}{})",
        d.name,
        d.scope,
        if d.ephemeral { ", ephemeral" } else { "" }
    );
    let why = d.check_error.as_deref().unwrap_or("no answer");
    out.push(match d.confirmed {
        Some(false) if d.gone => Line::bad(
            C,
            "device",
            format!("{what} is refused by the server: {why}"),
            if rep.remote_session && rep.authkey_set {
                "start a new session, which enrols again with RECALL_AUTHKEY"
            } else {
                "`recall connect`"
            },
        ),
        Some(false) => Line::warn(
            C,
            "device",
            format!("{what}, not confirmed by the server: {why}"),
            "Check again: `recall status` If it persists: `recall connect`",
        ),
        Some(true) => Line::good(
            C,
            "device",
            format!("{what}, confirmed by the server, key in {}", d.key_file),
        ),
        None => Line::good(C, "device", format!("{what}, key in {}", d.key_file)),
    });
}

/// Whether conflicting edits are merged, and what is waiting to be.
fn merge_lines(rep: &Report, out: &mut Vec<Line>) {
    use Group::Connection as C;
    const WORKER_LOGS: &str =
        "On the server, check the merge worker is running, and read its log: \
         `docker logs recall-worker`";
    out.push(match crate::doctor::worker_quiet(rep) {
        // What the worker last said about its CLI is only as current as
        // the worker: one that stopped asking for work is not ready,
        // whatever its last report was.
        Some(quiet) => Line::warn(
            C,
            "merge",
            format!(
                "stalled: the merge worker has not asked for work in {} minutes",
                quiet.whole_minutes()
            ),
            WORKER_LOGS,
        ),
        None => match (rep.merge_ready, rep.merge_worker) {
            (true, false) => Line::good(C, "merge", "ready, the server's claude CLI is logged in"),
            (true, true) => Line::good(
                C,
                "merge",
                "ready, through the merge worker, whose claude CLI is logged in",
            ),
            (false, false) => Line::warn(
                C,
                "merge",
                "not configured, so conflicting edits use last-write-wins",
                "On the server, log its Claude CLI in: \
                 `docker exec -it -u node recall-server claude setup-token`",
            ),
            (false, true) => Line::warn(
                C,
                "merge",
                "waiting: the merge worker's claude CLI is not logged in",
                "On the server, log its Claude CLI in: \
                 `docker exec -it -u node recall-worker claude setup-token`",
            ),
        },
    });
    let Some(q) = &rep.merge_queue else {
        return;
    };
    let waiting = |since: &str| format!("{} waiting, the oldest since {since}", q.queued);
    out.push(match q.oldest_queued_at.as_deref() {
        // An hour is long past what a running worker takes: say so here,
        // as doctor does, rather than leave the date to be read.
        Some(since)
            if crate::doctor::age_of(since).is_some_and(|age| age >= time::Duration::hours(1)) =>
        {
            Line::warn(C, "merge queue", waiting(since), WORKER_LOGS)
        }
        Some(since) => Line::good(C, "merge queue", waiting(since)),
        None => Line::good(C, "merge queue", "nothing waiting"),
    });
    if q.failed > 0 {
        out.push(Line::warn(
            C,
            "failed merges",
            format!(
                "{}; for each, the newest push stands and the merge is kept in its job",
                q.failed
            ),
            "GET /v1/jobs?state=failed, then POST /v1/jobs/{id}/retry, with the operator token",
        ));
    }
}

fn project_lines(rep: &Report, out: &mut Vec<Line>) {
    use Group::Project as P;
    out.push(if rep.in_git_repo {
        Line::good(P, "root", rep.project.clone())
    } else {
        Line::quiet(P, "root", format!("{}, not a git repository", rep.project))
    });

    let key = &rep.project_key;
    let mut key_line = match rep.project_key_source {
        KeySource::Declared => Line::good(
            P,
            "key",
            format!(
                "{key}, declared in RECALL_PROJECT_KEY, {}",
                match declared_in(rep, "RECALL_PROJECT_KEY") {
                    Some(file) => format!("set by {file}"),
                    None => "set in this shell".to_string(),
                }
            ),
        ),
        KeySource::Remote => Line::good(P, "key", format!("{key}, from the git remote")),
        KeySource::LocalPath if rep.in_git_repo => Line::warn(
            P,
            "key",
            format!(
                "{key}, from this checkout's path: with no git remote, another machine \
                 derives a different one"
            ),
            "Declare RECALL_PROJECT_KEY in .claude/settings.json, or add a remote: \
             `git remote add origin <url>`",
        ),
        KeySource::LocalPath => Line::quiet(P, "key", format!("{key}, from this directory's path")),
        KeySource::DeclaredButRejected => Line::bad(
            P,
            "key",
            format!("{key}, derived: RECALL_PROJECT_KEY is set but unusable, so it was ignored"),
            "make RECALL_PROJECT_KEY non-empty, free of whitespace, and not under 'global:'",
        ),
    };
    // An empty declaration never reaches `rejected_vars` (an empty value
    // reads as unset before anything can refuse it), so without this the
    // line would report a derived key while a settings file is plainly
    // trying to set it. The `declared` line below carries the fix.
    if let Some(file) = declared_in(rep, "RECALL_PROJECT_KEY") {
        if declared_empty(rep, "RECALL_PROJECT_KEY") {
            key_line.detail.push_str(&format!(
                "; RECALL_PROJECT_KEY is declared empty in {file}, which reads as unset"
            ));
        }
    }
    out.push(key_line);

    out.push(match (rep.hooks_wired, rep.in_git_repo) {
        (true, _) => Line::good(P, "hooks", "wired in .claude/settings.json"),
        (false, true) => Line::bad(
            P,
            "hooks",
            "not wired, so nothing here syncs",
            "`recall init`",
        ),
        (false, false) => Line::quiet(P, "hooks", "nothing to wire outside a git repository"),
    });
    out.push(
        match rep.memory_files {
            0 => Line::quiet(P, "memory", "no files yet"),
            n => Line::good(P, "memory", format!("{} on disk", files(n))),
        }
        .under(rep.memory_dir.clone()),
    );
    // Only when the server was asked: it was not without a credential.
    if rep.server_ok && rep.auth != "none" {
        out.push(Line::good(
            P,
            "synced",
            format!("{} on the server", files(rep.synced_files)),
        ));
    }
    if rep.remote_session {
        out.push(if rep.remote_memory_dir_set {
            Line::good(P, "remote memory", "CLAUDE_CODE_REMOTE_MEMORY_DIR is set")
        } else {
            Line::bad(
                P,
                "remote memory",
                "CLAUDE_CODE_REMOTE_MEMORY_DIR is not set, so auto-memory is off in this \
                 remote session",
                "set it to /home/user/.claude on the cloud environment (not $HOME)",
            )
        });
    }

    // Settings files, which is where a value actually comes from once one
    // declares it: the shell's is replaced, not consulted.
    for var in &rep.declared_env {
        let mut detail = format!("{} from {}", var.name, var.file);
        if var.empty {
            detail.push_str(", declared empty, so the setting is off");
        }
        if var.shadows_shell {
            detail.push_str(if var.empty {
                ", and it hides the value set in this shell"
            } else {
                ", overrides the value set in this shell"
            });
        }
        out.push(if var.empty {
            let fix = format!(
                "give {} a value in {}, or remove it there",
                var.name, var.file
            );
            Line::warn(P, "declared", detail, fix)
        } else {
            Line::good(P, "declared", detail)
        });
    }
    for var in &rep.ignored_env {
        out.push(Line::warn(
            P,
            "declared",
            format!(
                "{} in {} is not a string, so it sets nothing",
                var.name, var.file
            ),
            format!("quote its value in {}", var.file),
        ));
    }
    for file in &rep.unreadable_settings {
        out.push(Line::bad(
            P,
            "settings",
            format!(
                "{file} is not readable JSON, so nothing it declares is in effect; Claude \
                 Code cannot read it either"
            ),
            format!("fix the JSON in {file}"),
        ));
    }
}

/// The global and machine scopes, each asked the questions `recall doctor`
/// asks of them.
fn scope_lines(rep: &Report, out: &mut Vec<Line>) {
    use Group::Scopes as S;
    let scopes = [
        (
            "global",
            "RECALL_GLOBAL_KEY",
            scope::GLOBAL_DIR,
            &rep.global_key,
            rep.global_files,
            rep.global_linked,
            // Deliberately an invitation: sharing more is usually what
            // someone wants.
            "set RECALL_GLOBAL_KEY to share memories across projects",
        ),
        (
            "machine",
            "RECALL_MACHINE_KEY",
            scope::MACHINE_DIR,
            &rep.machine_key,
            rep.machine_files,
            rep.machine_linked,
            // Deliberately not an invitation: this content is true of one
            // machine only, and a cloud session, a new machine every time,
            // should leave it off.
            "name this machine with recall connect",
        ),
    ];
    for (label, var, dir, key, count, linked, when_off) in scopes {
        out.push(match key {
            None if rep.rejected_vars.contains(&var) => Line::bad(
                S,
                label,
                format!("off: {var} is set but empty once trimmed, so it was ignored"),
                format!("give {var} a value, or unset it"),
            ),
            // An empty *declaration* never reaches `rejected_vars`; the
            // `declared` line above says so and carries the fix.
            None if declared_empty(rep, var) => Line::quiet(
                S,
                label,
                format!(
                    "off, because {} declares {var} empty",
                    declared_in(rep, var).unwrap_or("a settings file")
                ),
            ),
            // Off with files under it: they are not filed under the
            // project either, so they sync nowhere at all.
            None if count > 0 => Line::warn(
                S,
                label,
                format!("off, but {} under {dir}/ sync nowhere", files(count)),
                format!("set {var}, or move the files out of {dir}/"),
            ),
            None => Line::quiet(S, label, format!("off; {when_off}")),
            Some(key) if count == 0 => Line::good(S, label, format!("{key}, no files yet")),
            Some(key) if linked => Line::good(
                S,
                label,
                format!("{key}, {} linked from MEMORY.md", files(count)),
            ),
            // The state the machine scope shipped in: the files arrive and
            // Claude Code never opens them, because it reads what
            // MEMORY.md links.
            Some(key) => Line::bad(
                S,
                label,
                format!(
                    "{key}, {}, but MEMORY.md links none of them, so Claude Code never reads \
                     them",
                    files(count)
                ),
                "recall pull",
            ),
        });
    }
    if let Some(d) = &rep.miscased_dir {
        out.push(Line::bad(
            S,
            "directory",
            format!(
                "{}/ is not {}/, so nothing under it syncs. On macOS the two are one directory \
                 and on Linux they are not, so Recall files it nowhere",
                d.found, d.reserved
            ),
            format!(
                "rename {}/ to {}/ in the memory directory",
                d.found, d.reserved
            ),
        ));
    }
}

/// This machine's witness of the server's audit log, in one line.
/// `server_ok`: whether the server answered at all, which the server's own
/// line has already reported when it did not.
fn audit_line(audit: &AuditReport, server_ok: bool) -> Line {
    use Group::History as H;
    const LABEL: &str = "audit log";
    if let Some(found) = &audit.inconsistent {
        return Line::bad(
            H,
            LABEL,
            format!(
                "rewritten: the server's log no longer extends a checkpoint saved here (found \
                 {}): {}{}",
                found.found_at,
                found.detail,
                if audit.unsaved.is_some() {
                    ", and this could not be saved to audit.json"
                } else {
                    ""
                }
            ),
            crate::audit::AFTER_A_REWRITE,
        );
    }
    if let Some(err) = &audit.file_error {
        return Line::bad(
            H,
            LABEL,
            format!("{err}; it may hold the only record of a rewrite"),
            format!(
                "look at {} first; move it aside only once you know what it held",
                audit.file
            ),
        );
    }
    if audit.dropped > 0 {
        return Line::bad(
            H,
            LABEL,
            format!(
                "{} checkpoint(s) dropped unchecked, a gap a rewrite could hide in",
                audit.dropped
            ),
            "`recall audit verify`",
        );
    }
    if let Some(why) = &audit.unproven {
        return Line::bad(
            H,
            LABEL,
            format!("not proven: the server answered without a proof: {why}"),
            "`recall audit verify`",
        );
    }
    if audit.extends == Some(true) {
        let newest = audit.newest.as_deref().map(short_checkpoint);
        return Line::good(
            H,
            LABEL,
            format!(
                "extends every checkpoint saved here ({} kept, newest {})",
                audit.checkpoints,
                newest.as_deref().unwrap_or("none")
            ),
        );
    }
    if let Some(err) = &audit.error {
        // Said once: an unreachable server is the server line's to report.
        let why = if server_ok || audit.refused {
            err.as_str()
        } else {
            "the server did not answer"
        };
        let detail = format!("not checked ({why}); {} checkpoint(s) saved", audit.saved());
        return match (audit.refused, audit.saved()) {
            (true, _) => Line::warn(
                H,
                LABEL,
                detail,
                "If the server no longer accepts this device: `recall connect`",
            ),
            // Nothing saved is nothing left unproven.
            (false, 0) => Line::quiet(H, LABEL, detail),
            (false, _) => Line::warn(
                H,
                LABEL,
                detail,
                "Once the server answers: `recall audit verify`",
            ),
        };
    }
    if audit.server_log == Some(false) && audit.saved() == 0 {
        return Line::quiet(H, LABEL, "not kept by this server (older than 0.4.2)");
    }
    if audit.saved() > 0 {
        return Line::warn(
            H,
            LABEL,
            format!("{} checkpoint(s) saved, not checked", audit.saved()),
            "`recall audit verify`",
        );
    }
    Line::quiet(H, LABEL, "nothing saved yet")
}

fn print_text(cfg: &ClientConfig, rep: &Report) {
    ui::title("recall status", &about(cfg));
    let lines = lines(cfg, rep);
    let width = lines.iter().map(|l| l.label.len()).max().unwrap_or(0);
    for group in Group::ALL {
        let items: Vec<&Line> = lines.iter().filter(|l| l.group == group).collect();
        if items.is_empty() {
            continue;
        }
        let about = match group {
            Group::Project => rep.project_key.as_str(),
            _ => "",
        };
        ui::section(group.name(), about);
        for l in items {
            ui::check_fitted(
                l.tone,
                l.label,
                width,
                &ui::tilde(&l.detail),
                l.fix.as_deref().map(ui::tilde).as_deref(),
            );
            if let Some(under) = &l.under {
                anstream::println!("{}{}", " ".repeat(width + 6), ui::dim(&ui::tilde(under)));
            }
        }
    }

    // The closing line, in `recall doctor`'s words, so the two commands
    // never describe the same state differently.
    let count = |tone| lines.iter().filter(|l| l.tone == tone).count();
    match (count(Tone::Bad), count(Tone::Warn)) {
        (0, 0) if !rep.in_git_repo => ui::verdict(
            Tone::Good,
            "Connected. Outside a git repository there is nothing here to sync.",
        ),
        (0, 0) if rep.server_ok && rep.auth != "none" => ui::verdict(
            Tone::Good,
            &format!(
                "{} syncs: {} here, {} on the server.",
                rep.project_key,
                files(rep.memory_files),
                rep.synced_files
            ),
        ),
        (0, 0) => ui::verdict(
            Tone::Good,
            &format!("{} is set up to sync.", rep.project_key),
        ),
        (0, w) => ui::verdict(
            Tone::Warn,
            &format!(
                "Nothing broken. {w} {} worth a look.",
                if w == 1 { "thing" } else { "things" }
            ),
        ),
        (b, _) => ui::verdict(
            Tone::Bad,
            &format!(
                "{b} {}. {}",
                if b == 1 { "problem" } else { "problems" },
                if rep.url_set {
                    "Some of it is not syncing."
                } else {
                    "Nothing syncs here."
                }
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// N2: a server slow at everything else is still asked for the audit
    /// check, on a budget of its own. Inside the server's budget, a slow
    /// `/health` kept the check from ever being asked, or counted as
    /// unanswered, so a stall like that never reached `recall doctor`'s
    /// rules. Mutation: ask it only when the rest finished in time.
    #[tokio::test]
    async fn the_audit_check_has_a_budget_of_its_own() {
        let root = recall_wire::audit::merkle::hash_leaf(b"x");
        let header = recall_hooks::audit::Checkpoint { size: 1, root }.header();
        let answer = recall_wire::AuditCheckpoint::parse_header_value(&header).unwrap();
        let app = axum::Router::new()
            .route(
                "/health",
                axum::routing::get(|| async {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    "late"
                }),
            )
            .route(
                recall_wire::audit::CHECKPOINT_PATH,
                axum::routing::get(move || {
                    let answer = answer.clone();
                    async move { axum::Json(answer) }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        let rep = collect_from(url).await;
        assert!(!rep.server_ok, "the rest ran out of time");
        let audit = rep.audit.unwrap();
        assert_eq!(audit.extends, Some(true), "{audit:?}");
        assert_eq!(audit.checkpoints, 1);
    }

    /// The report for a server at `url`, with a home of its own, a token,
    /// and short deadlines.
    async fn collect_from(url: String) -> Report {
        let dir = tempfile::tempdir().unwrap();
        let here = proj::resolve_at(dir.path().to_path_buf());
        let cfg = ClientConfig {
            url,
            token: "t".into(),
            audit_file: Some(dir.path().join("audit.json")),
            ..Default::default()
        };
        let deadlines = Deadlines {
            server: Duration::from_millis(300),
            audit: Duration::from_secs(10),
        };
        collect_within(&here, &cfg, deadlines).await
    }

    /// Every line of the text report that marks a problem carries what to
    /// run about it, the rule `recall doctor` keeps: a problem nobody can
    /// act on teaches the reader to skip the report. `Line::warn` and
    /// `Line::bad` take the fix; this is the guard on a line built some
    /// other way. Mutation: build one with `Line::new` and no fix.
    #[test]
    fn every_problem_line_says_what_to_run() {
        let healthy = crate::doctor::tests::healthy;
        let cfg = ClientConfig::default();
        let mut reports = Vec::new();

        let mut rep = healthy();
        rep.url_set = false;
        rep.url_source = Source::Unset;
        rep.token_set = false;
        rep.token_source = Source::Unset;
        rep.hooks_wired = false;
        rep.project_key_source = KeySource::DeclaredButRejected;
        rep.rejected_vars = vec!["RECALL_PROJECT_KEY", "RECALL_GLOBAL_KEY"];
        rep.unreadable_settings = vec!["/w/app/.claude/settings.json".into()];
        rep.miscased_dir = Some(MiscasedDir {
            found: "Global".into(),
            reserved: "global",
        });
        reports.push(rep);

        let mut rep = healthy();
        rep.server_ok = false;
        rep.server_error = Some("timed out".into());
        rep.project_key_source = KeySource::LocalPath;
        rep.credentials_exposed = true;
        rep.credentials_error = Some("bad TOML".into());
        rep.config_problems = vec!["unknown key 'sever'".into()];
        rep.machine_key = Some("machine:jarvis".into());
        rep.machine_files = 2;
        rep.global_files = 1;
        rep.device_error = Some("device.key is damaged".into());
        rep.device_file_exposed = true;
        rep.remote_session = true;
        rep.remote_memory_dir_set = false;
        rep.audit.as_mut().unwrap().dropped = 2;
        reports.push(rep);

        let mut rep = healthy();
        rep.merge_ready = false;
        rep.min_client = Some("9.0.0".into());
        rep.server_devices = Some(true);
        rep.merge_queue = Some(recall_wire::QueueStatus {
            queued: 3,
            oldest_queued_at: Some("2020-01-01T00:00:00.000Z".into()),
            failed: 1,
            ..Default::default()
        });
        rep.audit = Some(AuditReport {
            error: Some("forbidden".into()),
            refused: true,
            checkpoints: 1,
            ..Default::default()
        });
        reports.push(rep);

        let mut problems = 0;
        for rep in &reports {
            for line in lines(&cfg, rep) {
                if matches!(line.tone, Tone::Bad | Tone::Warn) {
                    problems += 1;
                    assert!(line.fix.is_some(), "a problem with no fix: {line:?}");
                }
            }
        }
        // Enough of them that the loop above is testing something.
        assert!(problems >= 20, "{problems}");
    }

    /// A healthy report is all good lines, closed by a count of what is
    /// here and on the server, and says nothing a person has to act on.
    #[test]
    fn a_healthy_report_has_nothing_to_act_on() {
        let rep = crate::doctor::tests::healthy();
        let lines = lines(&ClientConfig::default(), &rep);
        assert!(
            lines
                .iter()
                .all(|l| matches!(l.tone, Tone::Good | Tone::Quiet) && l.fix.is_none()),
            "{lines:#?}"
        );
        let audit = lines.iter().find(|l| l.label == "audit log").unwrap();
        assert!(
            audit.detail.contains("newest 1042 CsUYapGGPo4d…"),
            "the root is cut short: {audit:?}"
        );
    }

    #[test]
    fn a_checkpoint_and_a_commit_are_cut_short_for_reading() {
        assert_eq!(
            short_checkpoint("1042 CsUYapGGPo4dkMgIAUqom/Xajj7h2fB2MPA3j2jxq2I="),
            "1042 CsUYapGGPo4d…"
        );
        assert_eq!(short_checkpoint("7 short"), "7 short");
        assert_eq!(
            short_commit("0fa9e05aa1b2c3d4e5f60718293a4b5c6d7e8f90"),
            "0fa9e05aa1b2"
        );
        assert_eq!(short_commit("a1b2c3d"), "a1b2c3d");
    }

    /// A credential the audit routes refuse is reported as that, for
    /// `recall doctor` to say what to do about it. Mutation: report it as
    /// any other error.
    #[tokio::test]
    async fn a_refused_credential_is_flagged() {
        let app = axum::Router::new().route(
            recall_wire::audit::CHECKPOINT_PATH,
            axum::routing::get(|| async {
                (
                    axum::http::StatusCode::FORBIDDEN,
                    r#"{"error":"forbidden"}"#,
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        let audit = collect_from(url).await.audit.unwrap();
        assert!(audit.refused, "{audit:?}");
        assert!(
            audit.error.is_some() && audit.unproven.is_none(),
            "{audit:?}"
        );
    }
}
