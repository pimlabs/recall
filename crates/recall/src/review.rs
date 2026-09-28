//! `recall review run` and `recall review show` — checks what a memory
//! *claims* against what this checkout and its git history can see, and
//! says which claims still hold. See `docs/design/memory-truth.md`.
//!
//! **How loudly it may fail:** as quietly as `recall status`. A memory file
//! this cannot read, a directory that is not a git repository, a claim with
//! nothing here to check it against — none of that is an error. The review
//! offers evidence; it never refuses to run, and its exit code is always
//! [`exit::OK`] once it has run at all (`docs/design/memory-truth.md`'s
//! decision 4). The only non-zero exit is a genuine usage mistake, such as
//! naming a `[FILE]` that is not a memory file in any scope that is on.
//!
//! Layers 1 and 2: a candidate filter for dead artefact references, and
//! evidence from the checkout (`git ls-files`, `git log`, `git grep`, `git
//! tag`), from what this machine and its server say they are, from the
//! project's compose files and from a few environment variables. The last
//! four live in [`sources`], which says what they send and what they never
//! read; this file opens no connection of its own
//! (`this_module_never_touches_the_network`). `recall review apply` is a
//! later PR.
//!
//! Layer 3, only with `--claude`, lives in [`claude`]: the local `claude`
//! CLI over what layers 1 and 2 left undecided, handed a fact sheet of
//! what they observed and held to citing it. It is the only incremental
//! layer: layers 1 and 2 are cheap enough to run over every claim on every
//! run (`docs/design/memory-truth.md`'s "Incremental" section), while
//! layer 3 asks again only about a file whose content changed, or one
//! whose earlier answer cited a fact no longer observed, unless `--all`.
//!
//! **Scope matters to what can be checked.** A path, a script name or a
//! version only means something against *this* project's checkout — a
//! claim in the global or machine scope is not about this repository, so
//! those checks come back `cant_tell` there, whatever the checkout shows.
//! A `~/` or absolute path (or a Windows drive path) is checked against
//! this machine regardless of scope, since that is what it always means.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::Subcommand;
use recall_hooks::exit;
use serde::{Deserialize, Serialize};

use crate::project as proj;

mod claude;
mod sources;

/// `recall review …`.
#[derive(Subcommand)]
pub enum Cmd {
    /// Extract every checkable claim from memory and say which still hold
    Run {
        /// Only these files, relative to the memory directory (as `recall
        /// status` shows it); every file in every scope that is on, when
        /// left out. Naming a file that is not memory in an active scope
        /// is an error
        files: Vec<String>,
        /// With `--claude`, ask about every file again, not only those
        /// changed since claude was last asked. Layers 1 and 2 check every
        /// file on every run regardless
        #[arg(long)]
        all: bool,
        /// Also ask this machine's `claude` CLI (layer 3) about the claims
        /// layers 1 and 2 left undecided, one call per file. Spends your
        /// Claude usage; no API key is read or needed
        #[arg(long)]
        claude: bool,
        /// With `--claude`, the most files one run asks about; the rest are
        /// listed as skipped
        #[arg(long, value_name = "N", default_value_t = claude::DEFAULT_MAX_CALLS, requires = "claude")]
        max_calls: u32,
        /// Also ask each host a note names, other than this machine's
        /// server, for its discovery document (`GET /.well-known/recall`,
        /// no credential). Off by default: a note's text does not decide
        /// where this machine sends requests
        #[arg(long)]
        probe_hosts: bool,
        /// Machine-readable output, for scripts
        #[arg(long)]
        json: bool,
    },
    /// The last report again
    Show {
        /// Machine-readable output, for scripts
        #[arg(long)]
        json: bool,
    },
}

/// Runs one `recall review` command.
pub async fn run(cmd: Cmd) -> anyhow::Result<i32> {
    match cmd {
        Cmd::Run {
            files,
            all,
            claude,
            max_calls,
            probe_hosts,
            json,
        } => {
            let layer3 = claude.then_some(max_calls);
            run_review(&files, all, layer3, probe_hosts, json).await
        }
        Cmd::Show { json } => show_last(json),
    }
}

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

/// What a claim is, decided before anything judges it.
///
/// See `docs/design/memory-truth.md`'s classification table. The heuristic
/// leans toward [`Class::Record`] on purpose: calling a record present
/// pushes toward deleting history, and calling a present claim a record
/// only misses a stale line. Checked in this order — a record heading or
/// verb outranks a `type: user`/`feedback` note, so a correction written
/// inside the owner's own feedback still reads as history, not as a rule
/// to verify and never flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    /// A present-tense statement of state. Can go stale.
    Present,
    /// A heading, change verb, negated anchor or dated correction records
    /// what used to be true. Never stale, whatever the evidence.
    Record,
    /// A claim in a `type: user` or `type: feedback` note: the owner's own
    /// instruction. Substance is never judged; each anchor gets its own
    /// verdict instead of the claim getting one.
    Rule,
    /// None of the above. Only layer 3 (`--claude`) can judge it.
    Unsure,
    /// A merge left an inline `[CONFLICT: ...]` marker here. Reported, not
    /// judged.
    Conflict,
}

/// What a layer decided about a claim or an anchor, with the evidence that
/// decided it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Evidence contradicts it.
    Stale,
    /// Evidence confirms it.
    StillTrue,
    /// Nothing here can decide it, or it concerns another machine or scope.
    CantTell,
}

/// One thing read to decide an anchor's verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// Where this came from: `repository`, `git`, `machine`, `server`,
    /// `environment`, `compose`, `probe`, or `claude` (layer 3's reading).
    pub source: String,
    /// What was found, in a sentence.
    pub detail: String,
    /// This one anchor's own verdict. For a `present` claim these are
    /// aggregated into [`Claim::verdict`] too; for a `rule` claim this is
    /// the only verdict there is — the design gives every anchor of a rule
    /// its own verdict rather than the claim as a whole one, since a rule's
    /// substance is never judged.
    pub verdict: Verdict,
}

/// One claim: a list item, a sentence, or a fenced block, with the line
/// range it occupies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    /// `t1`, `t2`, … — never `f<n>`, so a claim's id cannot collide with a
    /// worker finding's when the two are shown together (a later PR).
    /// Reassigned on every run, in file-then-line order, so a merge with a
    /// restricted run's stored state never leaves a gap or a duplicate.
    pub id: String,
    /// The file this claim is in, relative to the memory directory, exactly
    /// as it is on disk (`global/editor.md`, `machine/ram.md`, `topics/x.md`).
    pub file: String,
    /// The 1-based line range this claim occupies.
    pub lines: [u32; 2],
    /// What kind of claim this is.
    pub class: Class,
    /// The claim's own text, for display; wrapped lines are joined with a
    /// single space, matching how Markdown reads them.
    pub text: String,
    /// The claim's own verdict: present claims only, aggregated across
    /// their anchors. Absent for a record (never judged), an unsure claim
    /// (waits for layer 3), a conflict (reported, not judged), and a rule
    /// (judged per anchor — see each entry in `evidence` instead).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    /// Which layer produced the verdict.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer: Option<u8>,
    /// What was read to decide it, one entry per anchor.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<Evidence>,
}

/// A source this design names that was not consulted, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnavailableSource {
    /// `repository`, `server`, `compose`, `probe` or `claude`.
    pub source: String,
    /// Why it was not read this run.
    pub reason: String,
}

/// What this run actually read, and what it did not. See
/// `docs/design/memory-truth.md`'s Output section.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourcesRead {
    /// The repository's `HEAD`, short form, when the project root is a git
    /// repository.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_head: Option<String>,
    /// The version the configured server's discovery document reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_version: Option<String>,
    /// The commit the configured server's `/health` reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_commit: Option<String>,
    /// The compose files read, relative to the repository root.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compose_files: Vec<String>,
    /// The hosts `--probe-hosts` asked. Empty without it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub probed: Vec<String>,
    /// Sources that could not be read this run, and why: the repository
    /// outside a git repository, the server when none is configured or it
    /// did not answer, the compose files when the checkout has none, and
    /// the hosts memory names when `--probe-hosts` was not given.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unavailable: Vec<UnavailableSource>,
    /// What layer 3 did, when `--claude` asked for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<ClaudeRun>,
}

/// What one `--claude` run did.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClaudeRun {
    /// How many times `claude` was called: one per file asked about.
    pub calls: u32,
    /// Files whose earlier answer still holds (unchanged, and every fact it
    /// cited still observed), so nothing was asked about them.
    pub reused: u32,
    /// Files with claims to decide that were not asked about, and why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedFile>,
}

/// A file layer 3 did not ask about, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedFile {
    /// The file, relative to the memory directory.
    pub file: String,
    /// Why it was skipped.
    pub reason: String,
}

/// The `--json` shape. Stable from its first release, under
/// `docs/reference/releasing.md`'s Versioning rules: a later PR may add a
/// field, never remove or repurpose one of these.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    /// When this review ran, RFC 3339.
    pub reviewed_at: String,
    /// The sources this run read, and what it could not.
    pub evidence: SourcesRead,
    /// Every claim this review holds, in file then line order: this run's
    /// files, merged with whatever a prior run held for files this run did
    /// not touch (see [`run_review`]).
    pub claims: Vec<Claim>,
}

/// What `.recall-review.json` holds: per-machine, never pushed, beside
/// `.recall-state.json`. See `claude::Env::review_file`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SavedState {
    /// Each reviewed file's content hash, as of the last run that touched
    /// it, and layer 3's last answer about it.
    #[serde(default)]
    files: BTreeMap<String, FileState>,
    /// The last report, so `recall review show` can print it again without
    /// re-reading memory.
    #[serde(default)]
    report: Option<Report>,
}

/// One file's bookkeeping in [`SavedState`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FileState {
    /// [`recall_wire::content_sha256`] of the content this review last read.
    content_sha256: String,
    /// Layer 3's answer, while it still holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    claude: Option<claude::Reviewed>,
}

fn load_state(path: &Path) -> SavedState {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_state(path: &Path, state: &SavedState) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
    std::fs::write(path, body)
}

// ---------------------------------------------------------------------------
// Running the review
// ---------------------------------------------------------------------------

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn join_relative(dir: &Path, rel: &str) -> PathBuf {
    let mut p = dir.to_path_buf();
    for part in rel.split('/').filter(|s| !s.is_empty()) {
        p.push(part);
    }
    p
}

/// Whether `rel` (relative to the memory directory) is a file the project
/// scope owns, rather than the global or machine scope.
fn is_project_scope(scopes: &[recall_hooks::Scope], rel: &str) -> bool {
    recall_hooks::scope::route(scopes, rel)
        .map(|(scope, _)| scope.prefix.is_none())
        .unwrap_or(true)
}

/// Runs layers 1 and 2 over every file in scope (or the files named), and
/// layer 3 when `layer3` holds `--max-calls`.
async fn run_review(
    file_args: &[String],
    all: bool,
    layer3: Option<u32>,
    probe_hosts: bool,
    json: bool,
) -> anyhow::Result<i32> {
    let here = proj::resolve();
    let cfg = here.config();
    let remote = proj::remote();
    let scopes = recall_hooks::scope::scopes(
        here.project_key(&cfg, &remote),
        cfg.global_key.clone(),
        cfg.machine_key.clone(),
    );
    let memory_dir = here.memory_dir();
    let review_file = here.review_file();

    let all_files: Vec<String> = recall_hooks::state::list_memory_files(&memory_dir)
        .unwrap_or_default()
        .into_iter()
        // `MEMORY.md` is the index, not a memory: the worker already checks
        // its links, and this design reviews claims, not link hygiene.
        .filter(|rel| rel != "MEMORY.md")
        // Only a file some active scope actually owns — this also drops a
        // miscased `Global/`-style directory, the same way `recall status`
        // never counts it as memory.
        .filter(|rel| recall_hooks::scope::route(&scopes, rel).is_some())
        .collect();

    let mut candidates = all_files.clone();
    if !file_args.is_empty() {
        let wanted: HashSet<&str> = file_args.iter().map(String::as_str).collect();
        let missing: Vec<&str> = wanted
            .iter()
            .copied()
            .filter(|f| !all_files.iter().any(|c| c == f))
            .collect();
        if !missing.is_empty() {
            let mut missing = missing;
            missing.sort_unstable();
            eprintln!(
                "recall review: not a memory file in any scope that is on: {}",
                missing.join(", ")
            );
            return Ok(exit::CONFIG);
        }
        candidates.retain(|f| wanted.contains(f.as_str()));
    }
    candidates.sort();

    let repo = Repo::at(proj::git_root());
    // Read once per run, not once per version anchor: the tag list does not
    // change while this command is running.
    let newest_tag = repo.newest_tag();

    // Extracted and classified first, judged after: which hosts
    // `--probe-hosts` asks depends on what the claims name.
    let mut extracted = Vec::new();
    let mut new_file_states = BTreeMap::new();
    let mut contents: BTreeMap<String, (String, bool)> = BTreeMap::new();
    for rel in &candidates {
        let is_project = is_project_scope(&scopes, rel);
        let path = join_relative(&memory_dir, rel);
        let Ok(content) = std::fs::read_to_string(&path) else {
            // Unreadable (removed mid-run, not UTF-8, a permissions race):
            // nothing to review here, and nothing worth failing the run
            // over — the next run sees it if it is still there.
            continue;
        };
        new_file_states.insert(
            rel.clone(),
            FileState {
                content_sha256: recall_wire::content_sha256(&content),
                claude: None,
            },
        );
        let note_type = front_matter_type(&content);
        for raw in extract_claims(&content) {
            let class = classify(&raw.text, raw.in_record_section, note_type.as_deref());
            extracted.push((rel.clone(), is_project, raw, class));
        }
        contents.insert(rel.clone(), (content, is_project));
    }

    let mut facts = sources::read(&here, &cfg, repo.root.as_deref()).await;
    let to_probe = sources::hosts_to_probe(
        extracted
            .iter()
            .map(|(_, _, raw, class)| (*class, raw.text.as_str())),
        &facts,
        probe_hosts,
    );
    facts.probes = sources::probe(&to_probe).await;

    let mut new_claims = Vec::new();
    for (rel, is_project, raw, class) in extracted {
        let (verdict, layer, evidence) =
            judge(class, &raw.text, &repo, is_project, &newest_tag, &facts);
        new_claims.push(Claim {
            id: String::new(),
            file: rel,
            lines: raw.lines,
            class,
            text: raw.text,
            verdict,
            layer,
            evidence,
        });
    }

    let mut saved = load_state(&review_file);

    let layer3_run = layer_three(
        &mut new_claims,
        &mut new_file_states,
        &saved,
        &contents,
        Layer3Inputs {
            repo: &repo,
            newest_tag: &newest_tag,
            facts: &facts,
            all,
            max_calls: layer3,
        },
    )
    .await;

    // Merge with whatever the last run held: a `[FILE]`-restricted run must
    // not make the other files' claims and hashes disappear from the
    // stored report, since `show` and any later run still need them.
    let processed: HashSet<&str> = candidates.iter().map(String::as_str).collect();
    let mut claims: Vec<Claim> = saved
        .report
        .as_ref()
        .map(|r| r.claims.clone())
        .unwrap_or_default()
        .into_iter()
        .filter(|c| !processed.contains(c.file.as_str()))
        .collect();
    claims.extend(new_claims);
    claims.sort_by(|a, b| a.file.cmp(&b.file).then(a.lines[0].cmp(&b.lines[0])));
    for (i, c) in claims.iter_mut().enumerate() {
        c.id = format!("t{}", i + 1);
    }
    for (rel, state) in new_file_states {
        saved.files.insert(rel, state);
    }

    let mut unavailable = Vec::new();
    let mut missing = |source: &str, reason: String| {
        unavailable.push(UnavailableSource {
            source: source.to_string(),
            reason,
        })
    };
    if repo.root.is_none() {
        missing("repository", "this is not a git repository".to_string());
    }
    match (&facts.server.url, facts.server.answered) {
        (None, _) => missing(
            "server",
            "no server is configured on this machine".to_string(),
        ),
        (Some(url), false) => missing(
            "server",
            format!(
                "/health at {url} did not answer ({})",
                facts.server.error.as_deref().unwrap_or("no reason given")
            ),
        ),
        (Some(_), true) => {}
    }
    if facts.compose.is_none() {
        missing(
            "compose",
            "the checkout has no deploy/docker-compose*.yml that reads as YAML".to_string(),
        );
    }
    if !probe_hosts {
        let named = sources::hosts_to_probe(
            claims.iter().map(|c| (c.class, c.text.as_str())),
            &facts,
            true,
        )
        .len();
        if named > 0 {
            missing(
                "probe",
                format!(
                    "{named} host(s) named in memory were not asked anything; --probe-hosts asks \
                     them"
                ),
            );
        }
    }
    if layer3.is_none() && layer3_run.waiting > 0 {
        missing(
            "claude",
            format!(
                "{} file(s) hold claims only layer 3 can decide; --claude asks this machine's \
                 claude CLI about them",
                layer3_run.waiting
            ),
        );
    }
    let report = Report {
        reviewed_at: now_rfc3339(),
        evidence: SourcesRead {
            repository_head: repo.head_short(),
            server_version: facts.server.version.clone(),
            server_commit: facts.server.commit.clone(),
            compose_files: facts
                .compose
                .as_ref()
                .map(|c| c.files.clone())
                .unwrap_or_default(),
            probed: facts.probes.keys().cloned().collect(),
            unavailable,
            claude: layer3.map(|_| layer3_run.run),
        },
        claims,
    };

    // Saved before it is printed, and a failed save is only a warning: the
    // report the owner is looking at never depends on the disk write
    // (decision 4: the exit code stays 0 whenever the review ran), and a
    // reader that stops early (`recall review run | head`) ends the process
    // at the first write to a closed pipe, which must not cost `recall
    // review show` the report.
    saved.report = Some(report);
    if let Err(e) = save_state(&review_file, &saved) {
        eprintln!(
            "recall review: this report was not saved ({e}); recall review show will not have \
             it until the next run succeeds"
        );
    }
    let report = saved.report.as_ref().expect("just set");
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
    } else {
        print_text(report);
    }
    Ok(exit::OK)
}

/// What layer 3 reads besides the claims themselves.
struct Layer3Inputs<'a> {
    repo: &'a Repo,
    newest_tag: &'a Option<(u64, u64, u64, String)>,
    facts: &'a sources::Facts,
    /// `--all`: ask about every file again.
    all: bool,
    /// `--max-calls`, when `--claude` asked for layer 3 at all.
    max_calls: Option<u32>,
}

/// What [`layer_three`] did.
struct Layer3Done {
    /// For the report, when `--claude` asked.
    run: ClaudeRun,
    /// Files with claims only layer 3 can decide and no answer that still
    /// holds: what `--claude` would ask about.
    waiting: usize,
}

/// A file layer 3 still has to ask about: its claims to decide, its fact
/// sheet, and, under `--all`, the earlier answer to fall back on if it is
/// not asked after all.
type ToAsk<'a> = (
    &'a String,
    Vec<claude::Asked>,
    Vec<String>,
    Option<claude::Reviewed>,
);

/// Whether layers 1 and 2 left `c` for layer 3: an `unsure` claim, or a
/// `present` one they could not decide.
fn undecided(c: &Claim) -> bool {
    c.class == Class::Unsure
        || (c.class == Class::Present && matches!(c.verdict, None | Some(Verdict::CantTell)))
}

/// Layer 3's fact sheet for the file `rel`: what this machine and its
/// checkout are, then everything layer 2 observed about this file's claims,
/// including each anchor of a claim it left `unsure`.
fn fact_sheet(
    global: &[String],
    claims: &[Claim],
    rel: &str,
    inputs: &Layer3Inputs<'_>,
    is_project: bool,
) -> Vec<String> {
    let mut sheet = global.to_vec();
    for c in claims.iter().filter(|c| c.file == rel) {
        if c.class == Class::Unsure {
            for a in anchors(&c.text) {
                let o = evidence_for(
                    &a,
                    inputs.repo,
                    is_project,
                    inputs.newest_tag,
                    &c.text,
                    inputs.facts,
                );
                sheet.push(o.detail);
            }
        } else {
            sheet.extend(
                c.evidence
                    .iter()
                    .filter(|e| e.source != claude::SOURCE)
                    .map(|e| e.detail.clone()),
            );
        }
    }
    claude::dedup(sheet)
}

/// Applies layer 3's answers about `rel` to the claims they were given for.
fn apply_decided(claims: &mut [Claim], rel: &str, decided: &[claude::Decided]) {
    for d in decided {
        if let Some(c) = claims
            .iter_mut()
            .find(|c| c.file == rel && c.lines == d.lines && c.text == d.text && undecided(c))
        {
            c.class = d.class;
            c.verdict = d.verdict;
            c.layer = Some(3);
            c.evidence.push(d.evidence.clone());
        }
    }
}

/// Layer 3 over the files this run read: an earlier answer is reused while
/// it holds, and, only when `--claude` asked, `claude` is called about each
/// file that still has claims to decide, up to `--max-calls`.
async fn layer_three(
    claims: &mut [Claim],
    states: &mut BTreeMap<String, FileState>,
    saved: &SavedState,
    contents: &BTreeMap<String, (String, bool)>,
    inputs: Layer3Inputs<'_>,
) -> Layer3Done {
    let mut global = Vec::new();
    if let Some(head) = inputs.repo.head_short() {
        global.push(format!("The project's repository is at commit {head}"));
    }
    if let Some((_, _, _, tag)) = inputs.newest_tag {
        global.push(format!(
            "The newest release tag in the project's repository is {tag}"
        ));
    }
    global.extend(inputs.facts.sheet());

    let mut run = ClaudeRun::default();
    let mut to_ask: Vec<ToAsk<'_>> = Vec::new();
    for (rel, (_, is_project)) in contents {
        let asked: Vec<claude::Asked> = claims
            .iter()
            .filter(|c| &c.file == rel && undecided(c))
            .map(|c| claude::Asked {
                lines: c.lines,
                text: c.text.clone(),
                class: c.class,
            })
            .collect();
        if asked.is_empty() {
            continue;
        }
        let sheet = fact_sheet(&global, claims, rel, &inputs, *is_project);
        let sha = states[rel].content_sha256.clone();
        let held = saved
            .files
            .get(rel)
            .and_then(|f| f.claude.clone())
            .filter(|r| r.holds(&sha, &sheet));
        match held {
            Some(r) if !(inputs.all && inputs.max_calls.is_some()) => {
                apply_decided(claims, rel, &r.decided);
                states.get_mut(rel).expect("read this run").claude = Some(r);
                run.reused += 1;
            }
            fallback => to_ask.push((rel, asked, sheet, fallback)),
        }
    }
    let waiting = to_ask.len();

    let Some(max_calls) = inputs.max_calls else {
        return Layer3Done { run, waiting };
    };
    if to_ask.is_empty() {
        return Layer3Done { run, waiting };
    }

    // Every file this run read teaches the redactor its secrets, so one
    // quoted in another note is masked too.
    let files: Vec<recall_wire::EvaluateFile> = contents
        .iter()
        .map(|(rel, (content, _))| recall_wire::EvaluateFile {
            file_path: rel.clone(),
            content: content.clone(),
            ..Default::default()
        })
        .collect();
    let redactor = recall_worker::Redactor::new(&files);
    let merger = claude::merger();
    let mut cannot_run: Option<String> = None;

    for (rel, asked, sheet, fallback) in to_ask {
        let (content, is_project) = &contents[rel];
        let answer = if let Some(why) = &cannot_run {
            Err(why.clone())
        } else if run.calls >= max_calls {
            Err(format!(
                "--max-calls {max_calls} reached; a later run asks about it"
            ))
        } else {
            let scope = if *is_project {
                "project scope: about this repository"
            } else {
                "global or machine scope: not about this repository"
            };
            let prompt = claude::prompt(rel, scope, content, &sheet, &asked, &redactor);
            if prompt.len() > claude::MAX_PROMPT_BYTES {
                Err(format!(
                    "its prompt is {} bytes, more than one call is handed ({})",
                    prompt.len(),
                    claude::MAX_PROMPT_BYTES
                ))
            } else {
                eprintln!("recall review: asking claude about {rel}");
                match merger.ask(claude::SYSTEM_PROMPT, &prompt).await {
                    Ok(text) => {
                        run.calls += 1;
                        claude::decide(&text, &asked, &sheet).ok_or_else(|| {
                            "claude's answer was not the JSON layer 3 asks for".to_string()
                        })
                    }
                    Err(recall_worker::merge::Error::Unavailable(why)) => {
                        let why = format!("claude cannot run on this machine: {why}");
                        cannot_run = Some(why.clone());
                        Err(why)
                    }
                    Err(e) => {
                        run.calls += 1;
                        Err(format!("the claude call failed: {e}"))
                    }
                }
            }
        };
        match answer {
            Ok(decided) => {
                apply_decided(claims, rel, &decided);
                let state = states.get_mut(rel).expect("read this run");
                state.claude = Some(claude::Reviewed {
                    content_sha256: state.content_sha256.clone(),
                    decided,
                });
            }
            Err(reason) => {
                run.skipped.push(SkippedFile {
                    file: rel.clone(),
                    reason,
                });
                if let Some(r) = fallback {
                    apply_decided(claims, rel, &r.decided);
                    states.get_mut(rel).expect("read this run").claude = Some(r);
                }
            }
        }
    }
    Layer3Done { run, waiting }
}

fn show_last(json: bool) -> anyhow::Result<i32> {
    let here = proj::resolve();
    let saved = load_state(&here.review_file());
    let Some(report) = saved.report else {
        // Valid JSON either way: a bare `null` is a complete JSON document,
        // and a script that reads `recall review show --json` before any
        // run has happened sees exactly that rather than an empty string or
        // an error.
        if json {
            println!("null");
        } else {
            println!("No review yet. recall review run makes one.");
        }
        return Ok(exit::OK);
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_text(&report);
    }
    Ok(exit::OK)
}

/// Layer 2, for a `present` or `rule` claim with anchors; everything else
/// gets no verdict at all in this PR (see [`Class`]'s doc). A `present`
/// claim's anchors are aggregated into one verdict; a `rule`'s are not —
/// each [`Evidence`] entry carries its own.
fn judge(
    class: Class,
    text: &str,
    repo: &Repo,
    is_project: bool,
    newest_tag: &Option<(u64, u64, u64, String)>,
    facts: &sources::Facts,
) -> (Option<Verdict>, Option<u8>, Vec<Evidence>) {
    if !matches!(class, Class::Present | Class::Rule) {
        // Layer 1's guard: a candidate in a record is dropped, not
        // verdicted, whatever it would otherwise look like. An unsure claim
        // waits for layer 3. A conflict is reported, not judged.
        return (None, None, Vec::new());
    }
    let found = anchors(text);
    if found.is_empty() {
        return (None, None, Vec::new());
    }
    let observed: Vec<Observed> = found
        .iter()
        .map(|a| evidence_for(a, repo, is_project, newest_tag, text, facts))
        .collect();
    let evidence: Vec<Evidence> = observed
        .iter()
        .map(|o| Evidence {
            source: o.source.clone(),
            detail: o.detail.clone(),
            verdict: verdict_of_signal(o.signal),
        })
        .collect();
    let verdict = match class {
        Class::Present => verdict_from(&observed),
        // A rule's substance is never judged as one claim; each anchor's
        // own verdict is already in `evidence` above.
        _ => None,
    };
    (verdict, Some(2), evidence)
}

// ---------------------------------------------------------------------------
// Claim extraction
// ---------------------------------------------------------------------------

/// One claim as extracted, before classification.
struct RawClaim {
    lines: [u32; 2],
    text: String,
    /// Whether the nearest heading above it reads like History, Previously
    /// or Correction — or is nested under one, at a deeper heading level.
    in_record_section: bool,
}

/// The line index just after the closing `---` of YAML front matter, or `0`
/// when the file has none.
fn front_matter_end(lines: &[&str]) -> usize {
    if lines.first().map(|l| l.trim()) != Some("---") {
        return 0;
    }
    for (i, line) in lines.iter().enumerate().skip(1) {
        if line.trim() == "---" {
            return i + 1;
        }
    }
    0
}

/// The front matter's `type:` value, if the file has front matter and a
/// `type:` line in it.
fn front_matter_type(content: &str) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let end = front_matter_end(&lines);
    if end == 0 {
        return None;
    }
    for line in &lines[..end] {
        if let Some(value) = line.trim().strip_prefix("type:") {
            return Some(value.trim().trim_matches(['"', '\'']).to_string());
        }
    }
    None
}

fn is_list_marker(t: &str) -> bool {
    if t.starts_with("- ") || t.starts_with("* ") || t.starts_with("+ ") {
        return true;
    }
    let digits: String = t.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return false;
    }
    let rest = &t[digits.len()..];
    rest.starts_with(". ") || rest.starts_with(") ")
}

fn is_block_boundary(t: &str) -> bool {
    t.is_empty() || t.starts_with('#') || t.starts_with("```") || t.starts_with("~~~")
}

/// Whether a heading's text reads like a section of history rather than
/// current state.
fn is_history_heading(heading: &str) -> bool {
    let lower = heading.to_ascii_lowercase();
    ["history", "previously", "correction", "corrections"]
        .iter()
        .any(|w| lower.contains(w))
}

/// A heading's level: how many leading `#` it has.
fn heading_level(trimmed: &str) -> usize {
    trimmed.chars().take_while(|&c| c == '#').count()
}

/// Splits a paragraph's lines into sentence-sized claims, each with the line
/// range it spans. Wrapped lines are joined with a single space, matching
/// how Markdown itself reads a soft-wrapped paragraph.
fn split_sentences(lines: &[&str], first_line: u32, in_record_section: bool) -> Vec<RawClaim> {
    let mut joined = String::new();
    let mut line_of: Vec<u32> = Vec::new();
    for (idx, l) in lines.iter().enumerate() {
        if idx > 0 {
            joined.push(' ');
            line_of.push(first_line + idx as u32);
        }
        for ch in l.chars() {
            joined.push(ch);
            line_of.push(first_line + idx as u32);
        }
    }
    let chars: Vec<char> = joined.chars().collect();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        let at_boundary =
            (c == '.' || c == '!' || c == '?') && (i + 1 == chars.len() || chars[i + 1] == ' ');
        if at_boundary {
            push_sentence(&chars, &line_of, start, i, in_record_section, &mut out);
            start = (i + 2).min(chars.len());
            i = start;
            continue;
        }
        i += 1;
    }
    if start < chars.len() {
        push_sentence(
            &chars,
            &line_of,
            start,
            chars.len() - 1,
            in_record_section,
            &mut out,
        );
    }
    out
}

fn push_sentence(
    chars: &[char],
    line_of: &[u32],
    start: usize,
    end: usize,
    in_record_section: bool,
    out: &mut Vec<RawClaim>,
) {
    if start > end || start >= chars.len() {
        return;
    }
    let end = end.min(chars.len() - 1);
    let text: String = chars[start..=end]
        .iter()
        .collect::<String>()
        .trim()
        .to_string();
    if text.is_empty() {
        return;
    }
    let l1 = line_of[start];
    let l2 = line_of[end];
    out.push(RawClaim {
        lines: [l1, l2],
        text,
        in_record_section,
    });
}

/// Extracts every claim from a memory file's body: one per list item, one
/// per sentence of a paragraph, one per fenced block. Front matter is
/// skipped.
fn extract_claims(content: &str) -> Vec<RawClaim> {
    let lines: Vec<&str> = content.lines().collect();
    let mut out = Vec::new();
    let mut i = front_matter_end(&lines);
    let mut in_record_section = false;
    // The level of the History-like heading currently in force, so a
    // nested `###` under a `## History` stays part of it, and a sibling
    // `##` heading correctly ends it.
    let mut record_heading_level: Option<usize> = None;
    while i < lines.len() {
        let raw = lines[i];
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            i += 1;
            continue;
        }
        if trimmed.starts_with('#') {
            let level = heading_level(trimmed);
            let heading = trimmed.trim_start_matches('#').trim();
            if is_history_heading(heading) {
                in_record_section = true;
                record_heading_level = Some(level);
            } else if record_heading_level.is_some_and(|rlevel| level > rlevel) {
                // A subheading nested under History — still history.
                in_record_section = true;
            } else {
                in_record_section = false;
                record_heading_level = None;
            }
            i += 1;
            continue;
        }
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let fence = &trimmed[..3];
            let open = i;
            let mut close = i;
            let mut j = i + 1;
            while j < lines.len() {
                if lines[j].trim_start().starts_with(fence) {
                    close = j;
                    break;
                }
                close = j;
                j += 1;
            }
            let text = lines[open..=close].join("\n");
            out.push(RawClaim {
                lines: [open as u32 + 1, close as u32 + 1],
                text,
                in_record_section,
            });
            i = close + 1;
            continue;
        }
        if is_list_marker(trimmed) {
            let open = i;
            let mut close = i;
            let mut j = i + 1;
            while j < lines.len() {
                let t = lines[j].trim();
                if is_block_boundary(t) || is_list_marker(t) {
                    break;
                }
                if lines[j].starts_with(' ') || lines[j].starts_with('\t') {
                    close = j;
                    j += 1;
                    continue;
                }
                break;
            }
            let joined = lines[open..=close]
                .join(" ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let text = strip_leading_marker(&joined).to_string();
            out.push(RawClaim {
                lines: [open as u32 + 1, close as u32 + 1],
                text,
                in_record_section,
            });
            i = close + 1;
            continue;
        }
        // A paragraph: every consecutive non-blank, non-list, non-heading,
        // non-fence line.
        let open = i;
        let mut close = i;
        let mut j = i + 1;
        while j < lines.len() {
            let t = lines[j].trim();
            if is_block_boundary(t) || is_list_marker(t) {
                break;
            }
            close = j;
            j += 1;
        }
        out.extend(split_sentences(
            &lines[open..=close],
            open as u32 + 1,
            in_record_section,
        ));
        i = close + 1;
    }
    out
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

const RECORD_MARKERS: &[&str] = &[
    "moved",
    "renamed",
    "replaced",
    "deleted",
    "retired",
    "no longer",
    "used to",
    "until ",
    "formerly",
    "was at",
    "is now",
    "since 0.",
    "since v",
];

const PRESENT_MARKERS: &[&str] = &[
    "is at",
    "is live",
    "lives in",
    "runs on",
    "deployed via",
    "is deployed",
    "is the sync server",
    "is currently",
    "currently uses",
    "is hosted",
];

fn looks_like_record(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("not `") || lower.contains("not \"") {
        return true;
    }
    RECORD_MARKERS.iter().any(|m| lower.contains(m))
}

fn strip_leading_marker(text: &str) -> &str {
    let t = text.trim_start();
    for prefix in ["- ", "* ", "+ "] {
        if let Some(rest) = t.strip_prefix(prefix) {
            return rest.trim_start();
        }
    }
    let digits: String = t.chars().take_while(char::is_ascii_digit).collect();
    if !digits.is_empty() {
        let rest = &t[digits.len()..];
        if let Some(after) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return after.trim_start();
        }
    }
    t
}

fn looks_like_present(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if PRESENT_MARKERS.iter().any(|m| lower.contains(m)) {
        return true;
    }
    let body = strip_leading_marker(text);
    if body.starts_with("Run `") || body.starts_with("run `") {
        return true;
    }
    // A line that is nothing but `NAME=value` states what a variable is
    // set to now: the design's own worked example has one
    // (`CLAUDE_CODE_REMOTE_MEMORY_DIR=/home/user/.claude`).
    let bare = body.trim().trim_end_matches('.').trim_matches('`');
    bare.split_once('=')
        .is_some_and(|(name, value)| is_envvarish(name) && !value.is_empty())
        && !bare.contains(char::is_whitespace)
}

/// Classifies one claim, given the note's own front-matter `type:` (if any)
/// and whether it sits under a history-like heading.
///
/// Order is load-bearing: a conflict marker wins outright; a record signal
/// (a heading, a change verb, a negated anchor) is checked *before* the
/// note's own `type:`, so a correction written inside a `type: feedback`
/// note — "not `lib.sh`, it was deleted" — reads as history and never gets
/// a stale verdict, rather than being treated as a rule whose anchors get
/// checked as if they were still being asserted.
fn classify(text: &str, in_record_section: bool, note_type: Option<&str>) -> Class {
    if text.contains("[CONFLICT") {
        return Class::Conflict;
    }
    if in_record_section || looks_like_record(text) {
        return Class::Record;
    }
    if matches!(note_type, Some("user") | Some("feedback")) {
        return Class::Rule;
    }
    if looks_like_present(text) {
        return Class::Present;
    }
    Class::Unsure
}

// ---------------------------------------------------------------------------
// Anchors
// ---------------------------------------------------------------------------

/// What kind of thing an anchor names, which decides how layer 2 checks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnchorKind {
    /// A repository path, or an absolute/`~/`/Windows-drive path on this
    /// machine.
    Path,
    /// A dotted hostname or a URL. Not checked until a later PR adds the
    /// server and discovery sources.
    Hostname,
    /// An `UPPER_SNAKE` variable, with or without `=value`. Not checked
    /// until a later PR adds the environment source.
    EnvVar,
    /// A SemVer version, checked against the newest release tag.
    SemVer,
    /// A `recall` subcommand or flag, or any other inline code whose
    /// referent might be a name in the tracked tree (`git grep`, excluding
    /// documentation).
    Code,
}

struct Anchor {
    kind: AnchorKind,
    value: String,
}

const KNOWN_EXTENSIONS: &[&str] = &[
    "sh", "bash", "zsh", "md", "rs", "py", "js", "ts", "json", "yml", "yaml", "toml", "ps1", "sql",
    "txt", "log", "service", "conf", "env", "lock", "ini", "cfg", "html", "css", "rb", "go",
];

fn has_file_extension(tok: &str) -> bool {
    tok.rfind('.')
        .map(|pos| &tok[pos + 1..])
        .is_some_and(|ext| {
            !ext.is_empty() && KNOWN_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
        })
}

/// A handful of `word/word` idioms and `number/unit` fractions that are not
/// paths, so `and/or` and `10/min` are not flagged as anchors just for
/// containing a slash.
fn looks_like_a_real_path_segment(tok: &str) -> bool {
    const NOT_PATHS: &[&str] = &[
        "and/or",
        "his/her",
        "he/she",
        "yes/no",
        "on/off",
        "true/false",
        "either/or",
        "pass/fail",
    ];
    if NOT_PATHS.contains(&tok.to_ascii_lowercase().as_str()) {
        return false;
    }
    if let Some((a, b)) = tok.split_once('/') {
        let numeric = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
        if numeric(a) || numeric(b) {
            return false;
        }
    }
    true
}

/// A Windows absolute path: a drive letter, a colon, then a separator
/// (`C:\Users\x` or `C:/Users/x`).
fn is_windows_path(tok: &str) -> bool {
    let b = tok.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

/// A path meaningful only on this machine: absolute, `~/`, or a Windows
/// drive path — never a repository path, whatever the checkout holds.
fn is_machine_absolute(value: &str) -> bool {
    value.starts_with('/') || value.starts_with("~/") || is_windows_path(value)
}

fn is_pathish(tok: &str) -> bool {
    // "`a.yml` / `b.yml`": a slash between two names is punctuation, and
    // the root directory is never what a note means by it.
    if tok.chars().all(|c| c == '/') {
        return false;
    }
    tok.starts_with('/')
        || tok.starts_with("~/")
        || tok.starts_with("./")
        || tok.starts_with("../")
        || is_windows_path(tok)
        || (tok.contains('/') && !tok.contains("://") && looks_like_a_real_path_segment(tok))
        || has_file_extension(tok)
}

fn is_semverish(tok: &str) -> bool {
    let t = tok.strip_prefix('v').unwrap_or(tok);
    let parts: Vec<&str> = t.splitn(4, '.').collect();
    parts.len() >= 3
        && parts[..3]
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

fn is_envvarish(tok: &str) -> bool {
    let name = tok.split('=').next().unwrap_or(tok);
    name.len() >= 3
        && name.contains('_')
        && name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn is_hostish(tok: &str) -> bool {
    if tok.starts_with("http://") || tok.starts_with("https://") {
        return true;
    }
    if tok.contains("://") {
        return false;
    }
    let t = tok.trim_end_matches('/');
    let parts: Vec<&str> = t.split('.').collect();
    parts.len() >= 2
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        && parts
            .last()
            .is_some_and(|tld| tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()))
}

/// Classifies one token (the inside of a backtick span when `in_code`, or a
/// bare word otherwise) as an anchor, if it is one at all. Outside code,
/// only the shapes explicitly named by the design carry meaning; a bare
/// English word is not an anchor just for being a word.
fn classify_anchor(tok: &str, in_code: bool) -> Option<Anchor> {
    let kind = if is_envvarish(tok) {
        AnchorKind::EnvVar
    } else if is_semverish(tok) {
        AnchorKind::SemVer
    } else if is_pathish(tok) {
        AnchorKind::Path
    } else if is_hostish(tok) {
        AnchorKind::Hostname
    } else if in_code || tok.starts_with("--") {
        // Inline code of no more specific shape (a script name, a crate, a
        // recall subcommand or flag) is still checkable: layer 2 asks
        // whether it is still referenced anywhere in the tracked tree,
        // outside documentation.
        AnchorKind::Code
    } else {
        return None;
    };
    Some(Anchor {
        kind,
        value: tok.to_string(),
    })
}

fn trim_token(tok: &str) -> &str {
    let tok = tok.trim_matches(|c: char| {
        !(c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '~' | '_' | '-' | '='))
    });
    tok.strip_suffix('.').unwrap_or(tok)
}

/// Every anchor a claim's text carries: backtick-delimited spans, always;
/// bare words outside them, only when they carry a recognisable shape.
/// De-duplicated by kind and value, so a claim naming the same anchor twice
/// gets one piece of evidence, not two identical ones.
fn anchors(text: &str) -> Vec<Anchor> {
    let mut out = Vec::new();
    let mut rest = text;
    let mut plain = String::new();
    while let Some(start) = rest.find('`') {
        plain.push_str(&rest[..start]);
        plain.push(' ');
        let after = &rest[start + 1..];
        let Some(end) = after.find('`') else {
            plain.push_str(after);
            rest = "";
            break;
        };
        let inner = after[..end].trim();
        if !inner.is_empty() {
            if inner.contains(' ') {
                if inner.starts_with("recall ") {
                    out.push(Anchor {
                        kind: AnchorKind::Code,
                        value: inner.to_string(),
                    });
                }
            } else if let Some(a) = classify_anchor(inner, true) {
                out.push(a);
            }
        }
        rest = &after[end + 1..];
    }
    plain.push_str(rest);
    for tok in plain.split_whitespace() {
        let tok = trim_token(tok);
        if tok.is_empty() {
            continue;
        }
        if let Some(a) = classify_anchor(tok, false) {
            out.push(a);
        }
    }
    out.sort_by(|a, b| (a.kind as u8, &a.value).cmp(&(b.kind as u8, &b.value)));
    out.dedup_by(|a, b| a.kind == b.kind && a.value == b.value);
    out
}

// ---------------------------------------------------------------------------
// Layer 2: evidence from the checkout and git
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signal {
    Confirms,
    Contradicts,
    Unknown,
}

fn verdict_of_signal(s: Signal) -> Verdict {
    match s {
        Signal::Confirms => Verdict::StillTrue,
        Signal::Contradicts => Verdict::Stale,
        Signal::Unknown => Verdict::CantTell,
    }
}

struct Observed {
    signal: Signal,
    source: String,
    detail: String,
}

fn observed(signal: Signal, source: &str, detail: String) -> Observed {
    Observed {
        signal,
        source: source.to_string(),
        detail,
    }
}

/// A pathspec for `value`, safe to pass to `git` after `--`.
///
/// A value with a slash is matched literally: git's default pathspec
/// matching treats `*`, `?` and `[` as wildcards even without `:(glob)`, so
/// a claim that happens to *quote* a glob such as `` `hooks/recall-*` ``
/// must not be matched against every file under `hooks/`. A bare filename —
/// what a note actually writes, most of the time — is matched anywhere in
/// the tree instead of only at the repository root, since `lib.sh` living
/// at `hooks/lib.sh` is exactly the case this exists for; a bare value that
/// itself contains a wildcard character falls back to a literal match,
/// which simply never exists, rather than glob-matching more than it
/// should.
fn pathspec_for(value: &str) -> String {
    if value.contains('/') || value.contains(['*', '?', '[']) {
        format!(":(literal){value}")
    } else {
        format!(":(glob)**/{value}")
    }
}

/// A thin wrapper over `git`, run in the project root. `root` is [`None`]
/// outside a git repository, in which case every check reads as
/// [`Signal::Unknown`] rather than running `git` at all.
///
/// Every invocation passes `stdin(Stdio::null())`: `git grep` in particular
/// has flags (`-f-`, `-O...`) that read from or act on things other than
/// its pattern, and a term taken verbatim from someone's memory file must
/// never be able to make this hang waiting on input that will never come.
struct Repo {
    root: Option<PathBuf>,
}

impl Repo {
    fn at(root: Option<PathBuf>) -> Self {
        Repo { root }
    }

    fn git(&self, args: &[&str]) -> Option<String> {
        let root = self.root.as_ref()?;
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .stdin(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn head_short(&self) -> Option<String> {
        self.git(&["rev-parse", "--short", "HEAD"])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// Whether `path` is tracked at `HEAD`.
    fn tracked(&self, path: &str) -> bool {
        let spec = pathspec_for(path);
        self.git(&["ls-files", "--", &spec])
            .is_some_and(|s| !s.trim().is_empty())
    }

    /// The commit and date `path` was deleted in, if git's history has one.
    fn deleted(&self, path: &str) -> Option<(String, String)> {
        let spec = pathspec_for(path);
        let out = self.git(&[
            "log",
            "--diff-filter=D",
            "-1",
            "--format=%h %cs",
            "--",
            &spec,
        ])?;
        let out = out.trim();
        if out.is_empty() {
            return None;
        }
        let (hash, date) = out.split_once(' ')?;
        Some((hash.to_string(), date.to_string()))
    }

    /// Whether `term` still appears anywhere in the tracked tree, outside
    /// Markdown documentation: a name mentioned only in `docs/**`,
    /// `ROADMAP.md` or `CHANGELOG.md` — including in the sentence *this*
    /// review is checking — is prose about the name, not the name still
    /// being used.
    ///
    /// `-e` marks `term` unambiguously as the pattern, never an option:
    /// without it, a term shaped like `-f-` makes `git grep` read patterns
    /// from standard input, `-fLICENSE` from a file, and `-O...` opens a
    /// pager — each on text taken verbatim from a memory file. `--w -F`
    /// (word-bounded, fixed-string) matches how this is documented; the
    /// trailing `--` closes off the option list before `-e`'s argument,
    /// belt-and-braces alongside `-e` itself.
    fn references(&self, term: &str) -> Option<bool> {
        let root = self.root.as_ref()?;
        let out = Command::new("git")
            .args(["grep", "-l", "-w", "-F", "-e", term, "--"])
            .current_dir(root)
            .stdin(Stdio::null())
            .output()
            .ok()?;
        // Exit 0: found in at least one file, listed on stdout. Exit 1:
        // ran, found nothing. Anything else (2+): the search itself failed
        // and says nothing about whether the term is still used.
        match out.status.code() {
            Some(0) => {
                let files = String::from_utf8_lossy(&out.stdout);
                Some(
                    files
                        .lines()
                        .any(|f| !f.trim().to_ascii_lowercase().ends_with(".md")),
                )
            }
            Some(1) => Some(false),
            _ => None,
        }
    }

    /// The newest tag that parses as a version, as `(major, minor, patch,
    /// the tag's own text)`.
    fn newest_tag(&self) -> Option<(u64, u64, u64, String)> {
        let out = self.git(&["tag"])?;
        out.lines()
            .filter_map(parse_semver_tag)
            .max_by_key(|t| (t.0, t.1, t.2))
    }
}

fn parse_semver(text: &str) -> Option<(u64, u64, u64)> {
    let t = text.strip_prefix('v').unwrap_or(text);
    let mut parts = t.splitn(3, '.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch_str = parts.next()?;
    let patch_digits: String = patch_str.chars().take_while(char::is_ascii_digit).collect();
    let patch = patch_digits.parse().ok()?;
    Some((major, minor, patch))
}

fn parse_semver_tag(tag: &str) -> Option<(u64, u64, u64, String)> {
    let (major, minor, patch) = parse_semver(tag.trim())?;
    Some((major, minor, patch, tag.trim().to_string()))
}

/// Whether the claim's own words assert that this *is* the current
/// version, rather than merely mentioning one — "shipped in 0.4.5" stays
/// true forever, even once 0.4.8 is out, and must never be marked stale
/// just for naming an older number.
fn asserts_current_version(claim_text: &str) -> bool {
    let lower = claim_text.to_ascii_lowercase();
    [
        "current version",
        "currently on",
        "is at version",
        "is on version",
        "is at v",
        "latest version",
        "version is",
        "recall is at",
    ]
    .iter()
    .any(|p| lower.contains(p))
}

fn machine_path_exists(p: &str) -> bool {
    let expanded = match p.strip_prefix("~/") {
        Some(rest) => match std::env::var("HOME") {
            Ok(home) if !home.is_empty() => format!("{home}/{rest}"),
            _ => return false,
        },
        None => p.to_string(),
    };
    Path::new(&expanded).exists()
}

fn evidence_for_path(repo: &Repo, value: &str, is_project: bool) -> Observed {
    if is_machine_absolute(value) {
        return if machine_path_exists(value) {
            observed(
                Signal::Confirms,
                "machine",
                format!("`{value}` exists on this machine"),
            )
        } else {
            observed(
                Signal::Unknown,
                "machine",
                format!(
                    "`{value}` was not found on this machine; the claim may be about another one"
                ),
            )
        };
    }
    if !is_project {
        return observed(
            Signal::Unknown,
            "repository",
            format!(
                "`{value}` names a repository path, but this note is outside the project scope; \
                 the checkout only speaks for the project"
            ),
        );
    }
    if repo.root.is_none() {
        return observed(
            Signal::Unknown,
            "repository",
            "this is not a git repository, so its history cannot be read".to_string(),
        );
    }
    if repo.tracked(value) {
        return observed(
            Signal::Confirms,
            "repository",
            format!("`{value}` exists at HEAD"),
        );
    }
    match repo.deleted(value) {
        Some((hash, date)) => observed(
            Signal::Contradicts,
            "git",
            format!("`{value}` was deleted in {hash} ({date}) and is not at HEAD"),
        ),
        None => observed(
            Signal::Unknown,
            "git",
            format!("`{value}` is not at HEAD and git has no record of it ever being tracked"),
        ),
    }
}

fn evidence_for_semver(
    newest_tag: &Option<(u64, u64, u64, String)>,
    value: &str,
    is_project: bool,
    claim_text: &str,
) -> Observed {
    if !is_project {
        return observed(
            Signal::Unknown,
            "repository",
            format!(
                "`{value}` would be checked against this project's release tags; this note is \
                 outside the project scope"
            ),
        );
    }
    let Some(claimed) = parse_semver(value) else {
        return observed(
            Signal::Unknown,
            "git",
            format!("`{value}` is not a version this can parse"),
        );
    };
    match newest_tag {
        Some((major, minor, patch, tag)) => {
            let newest = (*major, *minor, *patch);
            if claimed == newest {
                observed(
                    Signal::Confirms,
                    "git",
                    format!("matches the newest release tag {tag}"),
                )
            } else if claimed < newest && asserts_current_version(claim_text) {
                observed(
                    Signal::Contradicts,
                    "git",
                    format!("the newest release tag is {tag}"),
                )
            } else if claimed < newest {
                observed(
                    Signal::Unknown,
                    "git",
                    format!(
                        "older than the newest release tag {tag}; the claim may be about when \
                         something shipped, not the current version"
                    ),
                )
            } else {
                observed(
                    Signal::Unknown,
                    "git",
                    format!("newer than the newest release tag {tag}; nothing here can tell why"),
                )
            }
        }
        None => observed(Signal::Unknown, "git", "no release tags found".to_string()),
    }
}

fn evidence_for_code(repo: &Repo, value: &str, is_project: bool) -> Observed {
    if !is_project {
        return observed(
            Signal::Unknown,
            "repository",
            format!(
                "`{value}` would be checked against this project's tracked tree; this note is \
                 outside the project scope"
            ),
        );
    }
    match repo.references(value) {
        // Absence is weak evidence — a name can be real and simply not
        // grep-able (a different casing, a generated file) — so it is
        // never enough on its own to call a claim stale.
        Some(true) => observed(
            Signal::Confirms,
            "git",
            format!("`{value}` is still referenced in the tracked tree, outside documentation"),
        ),
        Some(false) => observed(
            Signal::Unknown,
            "git",
            format!(
                "`{value}` was not found outside documentation in the tracked tree; that alone \
                 does not make it gone"
            ),
        ),
        None => observed(
            Signal::Unknown,
            "git",
            "this is not a git repository, so it cannot be searched".to_string(),
        ),
    }
}

fn evidence_for(
    anchor: &Anchor,
    repo: &Repo,
    is_project: bool,
    newest_tag: &Option<(u64, u64, u64, String)>,
    claim_text: &str,
    facts: &sources::Facts,
) -> Observed {
    let value = anchor.value.as_str();
    match anchor.kind {
        AnchorKind::Path => evidence_for_path(repo, value, is_project),
        // A version in a claim about the server is about what the server
        // runs, whatever the scope: the server is this machine's, not the
        // project's. Any other version is about this project's releases.
        AnchorKind::SemVer => parse_semver(value)
            .and_then(|v| sources::evidence_for_server_version(v, claim_text, facts))
            .unwrap_or_else(|| evidence_for_semver(newest_tag, value, is_project, claim_text)),
        AnchorKind::Code => sources::evidence_for_name(value, facts, is_project)
            .unwrap_or_else(|| evidence_for_code(repo, value, is_project)),
        AnchorKind::Hostname => sources::evidence_for_host(value, claim_text, facts),
        AnchorKind::EnvVar => {
            if let Some(o) = sources::evidence_for_env_value(value, claim_text, facts) {
                return o;
            }
            // Any other variable is judged by its name: a value it is given
            // in a note may be a secret, and is never read or compared.
            let name = value.split('=').next().unwrap_or(value);
            sources::evidence_for_env_name(name, facts, is_project)
                .unwrap_or_else(|| evidence_for_code(repo, name, is_project))
        }
    }
}

fn verdict_from(observed: &[Observed]) -> Option<Verdict> {
    if observed.is_empty() {
        return None;
    }
    if observed.iter().any(|o| o.signal == Signal::Contradicts) {
        Some(Verdict::Stale)
    } else if observed.iter().all(|o| o.signal == Signal::Confirms) {
        Some(Verdict::StillTrue)
    } else {
        Some(Verdict::CantTell)
    }
}

// ---------------------------------------------------------------------------
// Human output
// ---------------------------------------------------------------------------

fn lines_desc(l: &[u32; 2]) -> String {
    if l[0] == l[1] {
        format!("L{}", l[0])
    } else {
        format!("L{}-{}", l[0], l[1])
    }
}

/// Memory text, made safe to write to a terminal: a control character
/// (other than a tab) becomes a space, so a note nobody has looked at
/// cannot plant an escape sequence that moves the cursor or rewrites what
/// came before it. Never applied to the stored or `--json` text — only to
/// what actually reaches a terminal.
fn sanitize_for_terminal(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() && c != '\t' { ' ' } else { c })
        .collect()
}

fn print_text(rep: &Report) {
    crate::ui::title(
        "recall review",
        rep.evidence.repository_head.as_deref().unwrap_or(""),
    );
    println!();
    if rep.claims.is_empty() {
        println!("Nothing to review: no memory files with checkable content were found.");
        return;
    }

    let mut by_file: BTreeMap<&str, Vec<&Claim>> = BTreeMap::new();
    for c in &rep.claims {
        by_file.entry(c.file.as_str()).or_default().push(c);
    }

    let (mut stale, mut still_true, mut cant_tell, mut conflicts, mut records, mut unsure) =
        (0, 0, 0, 0, 0, 0);

    for (file, claims) in &by_file {
        println!("{file}");
        for c in claims.iter().filter(|c| c.class == Class::Conflict) {
            println!(
                "  {:<10} {:<7} {}",
                "conflict",
                lines_desc(&c.lines),
                sanitize_for_terminal(&c.text)
            );
            conflicts += 1;
        }
        for c in claims.iter().filter(|c| c.class == Class::Rule) {
            println!(
                "  {:<10} {:<7} {}",
                "rule",
                lines_desc(&c.lines),
                sanitize_for_terminal(&c.text)
            );
            for e in &c.evidence {
                let tag = match e.verdict {
                    Verdict::Stale => "stale",
                    Verdict::StillTrue => "still_true",
                    Verdict::CantTell => "cant_tell",
                };
                println!("             [{tag}] {}", sanitize_for_terminal(&e.detail));
                match e.verdict {
                    Verdict::Stale => stale += 1,
                    Verdict::StillTrue => still_true += 1,
                    Verdict::CantTell => cant_tell += 1,
                }
            }
        }
        for verdict in [Verdict::Stale, Verdict::CantTell, Verdict::StillTrue] {
            for c in claims
                .iter()
                .filter(|c| c.class == Class::Present && c.verdict == Some(verdict))
            {
                let tag = match verdict {
                    Verdict::Stale => "stale",
                    Verdict::CantTell => "cant_tell",
                    Verdict::StillTrue => "still_true",
                };
                match verdict {
                    Verdict::Stale => stale += 1,
                    Verdict::CantTell => cant_tell += 1,
                    Verdict::StillTrue => still_true += 1,
                }
                println!(
                    "  {:<10} {:<7} {}",
                    tag,
                    lines_desc(&c.lines),
                    sanitize_for_terminal(&c.text)
                );
                for e in &c.evidence {
                    println!("             {}", sanitize_for_terminal(&e.detail));
                }
            }
        }
        let file_records = claims.iter().filter(|c| c.class == Class::Record).count();
        let file_unsure = claims.iter().filter(|c| c.class == Class::Unsure).count();
        records += file_records;
        unsure += file_unsure;
        if file_records > 0 {
            println!("  {file_records} record(s), not reviewed");
        }
        if file_unsure > 0 {
            println!("  {file_unsure} claim(s) with nothing here to decide them");
        }
        println!();
    }

    println!(
        "{stale} stale, {still_true} still true, {cant_tell} cant tell, {conflicts} conflict(s), \
         {records} record(s), {unsure} unresolved"
    );

    // Layer 3's own account: what it asked, and what it did not, never
    // silently cut.
    if let Some(run) = &rep.evidence.claude {
        println!(
            "claude: {} call(s), {} file(s) unchanged since it was last asked",
            run.calls, run.reused
        );
        for s in &run.skipped {
            println!(
                "  skipped {}: {}",
                sanitize_for_terminal(&s.file),
                sanitize_for_terminal(&s.reason)
            );
        }
    } else if let Some(u) = rep
        .evidence
        .unavailable
        .iter()
        .find(|u| u.source == "claude")
    {
        println!("{}", u.reason);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims_of(content: &str) -> Vec<(Class, String, [u32; 2])> {
        let note_type = front_matter_type(content);
        extract_claims(content)
            .into_iter()
            .map(|raw| {
                let class = classify(&raw.text, raw.in_record_section, note_type.as_deref());
                (class, raw.text, raw.lines)
            })
            .collect()
    }

    /// A local git repository with `git config` already set, for tests that
    /// need real commits.
    fn git_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "Test"],
        ] {
            assert!(Command::new("git")
                .args(&args)
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success());
        }
        dir
    }

    fn git_commit(dir: &Path, message: &str) {
        assert!(Command::new("git")
            .args(["add", "-A"])
            .current_dir(dir)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args(["commit", "-q", "--allow-empty", "-m", message])
            .current_dir(dir)
            .status()
            .unwrap()
            .success());
    }

    #[test]
    fn a_list_item_is_one_claim_with_its_line_range() {
        let content = "- The server is at `x`.\n- Second one.\n";
        let claims = claims_of(content);
        assert_eq!(claims.len(), 2, "{claims:?}");
        assert_eq!(claims[0].2, [1, 1]);
        assert_eq!(claims[1].2, [2, 2]);
    }

    #[test]
    fn a_fenced_block_is_one_claim() {
        let content = "Some text.\n\n```sh\nrecall status\necho hi\n```\n";
        let claims = claims_of(content);
        let fence = claims
            .iter()
            .find(|c| c.1.contains("```"))
            .expect("a fenced claim");
        assert_eq!(fence.2, [3, 6], "{:?}", fence.2);
    }

    #[test]
    fn front_matter_is_never_a_claim() {
        let content = "---\nname: x\ntype: user\n---\n\nPrefers tabs.\n";
        let claims = claims_of(content);
        assert!(claims.iter().all(|c| !c.1.contains("type: user")));
    }

    #[test]
    fn a_type_user_note_classifies_every_claim_as_rule() {
        let content = "---\ntype: user\n---\n\nPrefers tabs over spaces.\n";
        let claims = claims_of(content);
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].0, Class::Rule);
    }

    /// The order classification checks in: a record signal wins over the
    /// note's own `type:`, so a correction inside feedback is never treated
    /// as a rule whose anchors get judged as still-current.
    #[test]
    fn a_correction_inside_a_feedback_note_is_a_record_not_a_rule() {
        let content =
            "---\ntype: feedback\n---\n\nNot `lib.sh` any more, it was deleted in the rewrite.\n";
        let claims = claims_of(content);
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].0, Class::Record, "{:?}", claims[0]);
    }

    #[test]
    fn a_history_heading_makes_every_claim_under_it_a_record() {
        let content = "It is at `x`.\n\n## History\n\nIt used to be at `y`.\n";
        let claims = claims_of(content);
        assert_eq!(claims.len(), 2);
        assert_eq!(claims[0].0, Class::Present, "{:?}", claims[0]);
        assert_eq!(claims[1].0, Class::Record, "{:?}", claims[1]);
    }

    /// A subsection nested under a History heading stays part of it, and a
    /// sibling heading at the same level correctly leaves it.
    #[test]
    fn a_nested_heading_under_history_stays_a_record() {
        let content =
            "## History\n\n### 2026-09\n\nIt used to be at `y`.\n\n## Now\n\nIt is at `x`.\n";
        let claims = claims_of(content);
        assert_eq!(claims.len(), 2);
        assert_eq!(
            claims[0].0,
            Class::Record,
            "nested under History: {:?}",
            claims[0]
        );
        assert_eq!(
            claims[1].0,
            Class::Present,
            "back to a sibling section: {:?}",
            claims[1]
        );
    }

    #[test]
    fn a_negated_anchor_is_a_record_even_without_a_heading() {
        let content =
            "Not the old `hooks/recall-pull` script, and not `lib.sh`: both were deleted.\n";
        let claims = claims_of(content);
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].0, Class::Record);
    }

    #[test]
    fn a_conflict_marker_is_its_own_class_whatever_else_is_true() {
        let content = "The rate limit is `[CONFLICT: 10/min vs 100/min]` per minute.\n";
        let claims = claims_of(content);
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].0, Class::Conflict);
    }

    #[test]
    fn plain_prose_with_no_present_or_record_signal_is_unsure() {
        let content = "Something about the weather today, which is nice.\n";
        let claims = claims_of(content);
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].0, Class::Unsure);
    }

    /// The design's worked example writes one claim as a bare assignment
    /// (`CLAUDE_CODE_REMOTE_MEMORY_DIR=/home/user/.claude`): that states
    /// what a variable is set to now. Prose that merely contains one does
    /// not become present for it.
    #[test]
    fn a_bare_assignment_is_a_present_claim() {
        for line in [
            "- CLAUDE_CODE_REMOTE_MEMORY_DIR=/home/user/.claude\n",
            "- `RECALL_URL=https://recall.example.com`.\n",
        ] {
            assert_eq!(claims_of(line)[0].0, Class::Present, "{line:?}");
        }
        for line in [
            "- Maybe try FOO_BAR=1 when it misbehaves.\n",
            "- FOO_BAR=\n",
        ] {
            assert_eq!(claims_of(line)[0].0, Class::Unsure, "{line:?}");
        }
    }

    #[test]
    fn anchors_recognise_every_kind_the_design_names() {
        let text =
            "`lib.sh` `recall.pimlabs.id` `RECALL_URL` `0.4.8` `--json` `/home/user/.claude`";
        let found = anchors(text);
        let kinds: Vec<AnchorKind> = found.iter().map(|a| a.kind).collect();
        assert!(kinds.contains(&AnchorKind::Path), "{kinds:?}");
        assert!(kinds.contains(&AnchorKind::Hostname), "{kinds:?}");
        assert!(kinds.contains(&AnchorKind::EnvVar), "{kinds:?}");
        assert!(kinds.contains(&AnchorKind::SemVer), "{kinds:?}");
        assert!(kinds.contains(&AnchorKind::Code), "{kinds:?}");
    }

    #[test]
    fn a_bare_english_word_outside_code_is_not_an_anchor() {
        let found = anchors("recall is a tool that syncs memory reliably across machines");
        assert!(
            found.is_empty(),
            "{:?}",
            found.iter().map(|a| &a.value).collect::<Vec<_>>()
        );
    }

    /// The cheap false positives a bare slash-based path rule would create.
    #[test]
    fn common_idioms_and_fractions_are_not_path_anchors() {
        for tok in ["and/or", "10/min", "on/off", "1/2", "/", "//"] {
            assert!(!is_pathish(tok), "{tok:?} should not look like a path");
        }
        assert!(is_pathish("hooks/recall-pull"), "a real path still does");
    }

    #[test]
    fn a_windows_drive_path_is_a_path_anchor_not_code() {
        for tok in [r"C:\Users\x\.claude", "C:/Users/x/.claude"] {
            let found = anchors(&format!("`{tok}`"));
            assert_eq!(
                found.len(),
                1,
                "{tok:?}: {:?}",
                found.iter().map(|a| &a.value).collect::<Vec<_>>()
            );
            assert_eq!(found[0].kind, AnchorKind::Path, "{tok:?}");
        }
    }

    /// With no server configured there is nothing to compare a hostname
    /// with, and nothing is asked of it.
    #[test]
    fn a_hostname_with_no_server_configured_cant_tell() {
        let repo = Repo::at(None);
        let o = evidence_for(
            &Anchor {
                kind: AnchorKind::Hostname,
                value: "recall.pimlabs.id".into(),
            },
            &repo,
            true,
            &None,
            "",
            &sources::Facts::default(),
        );
        assert_eq!(o.signal, Signal::Unknown);
        assert_eq!(o.source, "server");
    }

    /// The mutation this pins against: a record's anchors must never reach
    /// [`judge`]'s evidence-gathering at all, whatever the evidence would
    /// say. Dropping the class gate in `judge` — treating a layer 1
    /// candidate as if it were a verdict-worthy present claim — must make
    /// this fail.
    #[test]
    fn a_record_never_gets_a_verdict() {
        let repo = Repo::at(None);
        let (verdict, layer, evidence) = judge(
            Class::Record,
            "Not the old `hooks/recall-pull` script, and not `lib.sh`.",
            &repo,
            true,
            &None,
            &sources::Facts::default(),
        );
        assert_eq!(verdict, None);
        assert_eq!(layer, None);
        assert!(evidence.is_empty());
    }

    #[test]
    fn an_unsure_claim_gets_no_verdict_in_this_pr() {
        let repo = Repo::at(None);
        let (verdict, layer, _) = judge(
            Class::Unsure,
            "`lib.sh` is mentioned here.",
            &repo,
            true,
            &None,
            &sources::Facts::default(),
        );
        assert_eq!(verdict, None);
        assert_eq!(layer, None);
    }

    /// The design's own distinction: a `present` claim gets one verdict for
    /// the whole claim, but a `rule`'s substance is never judged — only its
    /// anchors are, each with a verdict of its own.
    #[test]
    fn a_rule_gets_no_claim_level_verdict_but_each_anchor_does() {
        let dir = git_fixture();
        std::fs::write(dir.path().join("lib.sh"), "echo hi\n").unwrap();
        git_commit(dir.path(), "add");
        std::fs::remove_file(dir.path().join("lib.sh")).unwrap();
        git_commit(dir.path(), "remove");
        let repo = Repo::at(Some(dir.path().to_path_buf()));
        let (verdict, _layer, evidence) = judge(
            Class::Rule,
            "Use `lib.sh` for this.",
            &repo,
            true,
            &None,
            &sources::Facts::default(),
        );
        assert_eq!(verdict, None, "a rule's substance is never judged as one");
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].verdict, Verdict::Stale, "{evidence:?}");
    }

    /// Scope gating: a global or machine note is not about this
    /// repository, so a repository-shaped check on it is `cant_tell`
    /// whatever the checkout actually shows.
    #[test]
    fn a_non_project_scope_gets_cant_tell_from_repository_checks() {
        let dir = git_fixture();
        std::fs::write(dir.path().join("lib.sh"), "echo hi\n").unwrap();
        git_commit(dir.path(), "add");
        std::fs::remove_file(dir.path().join("lib.sh")).unwrap();
        git_commit(dir.path(), "remove");
        let repo = Repo::at(Some(dir.path().to_path_buf()));
        let o = evidence_for_path(&repo, "lib.sh", false);
        assert_eq!(
            o.signal,
            Signal::Unknown,
            "{o:?}",
            o = (o.source.clone(), o.detail.clone())
        );
    }

    /// A version mentioned as history ("shipped in an older version") must
    /// never be marked stale just because a newer release exists now.
    #[test]
    fn an_older_version_is_only_stale_when_the_claim_asserts_its_current() {
        let dir = git_fixture();
        git_commit(dir.path(), "x");
        assert!(Command::new("git")
            .args(["tag", "v0.4.8"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
        let newest = Repo::at(Some(dir.path().to_path_buf())).newest_tag();

        let historical = evidence_for_semver(
            &newest,
            "0.4.5",
            true,
            "`--contradictions` shipped in 0.4.5.",
        );
        assert_eq!(historical.signal, Signal::Unknown, "{}", historical.detail);

        let current = evidence_for_semver(&newest, "0.4.5", true, "Recall is at version 0.4.5.");
        assert_eq!(current.signal, Signal::Contradicts, "{}", current.detail);
    }

    #[test]
    fn semver_evidence_confirms_the_newest_tag() {
        let dir = git_fixture();
        git_commit(dir.path(), "x");
        assert!(Command::new("git")
            .args(["tag", "v0.4.8"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
        let newest = Repo::at(Some(dir.path().to_path_buf())).newest_tag();
        let current = evidence_for_semver(&newest, "0.4.8", true, "Recall is at version 0.4.8.");
        assert_eq!(current.signal, Signal::Confirms, "{}", current.detail);
    }

    /// The blocker this pins against: none of these is a `git grep` option,
    /// whatever it looks like — `-e` makes each one an ordinary pattern,
    /// which is why every one of them correctly reads as "not found"
    /// rather than hanging on stdin, reading a file, opening a pager, or
    /// making `references` misreport "not a git repository".
    #[test]
    fn hostile_terms_are_never_interpreted_as_git_options() {
        let dir = git_fixture();
        std::fs::write(dir.path().join("code.rs"), "fn main() {}\n").unwrap();
        git_commit(dir.path(), "x");
        let repo = Repo::at(Some(dir.path().to_path_buf()));
        for hostile in [
            "-f-",
            "-fLICENSE",
            "--contradictions",
            "-O/bin/sh",
            "-x",
            "-h",
        ] {
            let result = repo.references(hostile);
            assert_eq!(
                result,
                Some(false),
                "hostile term {hostile:?} must read as not-found, not as a git option"
            );
        }
    }

    /// A flag this codebase actually uses is found — proving `references`
    /// can say `Confirms` at all, not just fail safe.
    #[test]
    fn a_flag_that_is_still_referenced_is_confirmed() {
        let dir = git_fixture();
        std::fs::write(dir.path().join("code.rs"), "let flag = \"--keep-open\";\n").unwrap();
        git_commit(dir.path(), "x");
        let repo = Repo::at(Some(dir.path().to_path_buf()));
        assert_eq!(repo.references("--keep-open"), Some(true));
    }

    /// A name mentioned only in a Markdown file — documentation talking
    /// about something, not code using it — must not count as "still
    /// referenced", or every dead name this design exists to catch would
    /// read as still true because the note itself (or the ROADMAP, or
    /// history docs) says its name somewhere.
    #[test]
    fn a_name_mentioned_only_in_markdown_is_not_confirmed() {
        let dir = git_fixture();
        std::fs::write(
            dir.path().join("notes.md"),
            "hooks/recall-pull used to exist\n",
        )
        .unwrap();
        git_commit(dir.path(), "x");
        let repo = Repo::at(Some(dir.path().to_path_buf()));
        assert_eq!(repo.references("recall-pull"), Some(false));
    }

    /// Absence of a code term is weak evidence: it must read `cant_tell`,
    /// never `stale`, however confidently `git grep` comes back empty.
    #[test]
    fn an_absent_code_term_is_cant_tell_never_stale() {
        let dir = git_fixture();
        git_commit(dir.path(), "x");
        let repo = Repo::at(Some(dir.path().to_path_buf()));
        let o = evidence_for_code(&repo, "never-existed-anywhere", true);
        assert_eq!(o.signal, Signal::Unknown, "{}", o.detail);
    }

    /// A bare filename in a note (what people actually write) still finds
    /// the real file, wherever it lives in the tree — this is the failure
    /// the design's own worked example depends on: `lib.sh` really lived at
    /// `hooks/lib.sh`.
    #[test]
    fn a_bare_filename_matches_the_real_file_in_a_subdirectory() {
        let dir = git_fixture();
        std::fs::create_dir_all(dir.path().join("hooks")).unwrap();
        std::fs::write(dir.path().join("hooks").join("lib.sh"), "echo hi\n").unwrap();
        git_commit(dir.path(), "add");
        let repo = Repo::at(Some(dir.path().to_path_buf()));
        assert!(
            repo.tracked("lib.sh"),
            "bare lib.sh should find hooks/lib.sh"
        );
        std::fs::remove_file(dir.path().join("hooks").join("lib.sh")).unwrap();
        git_commit(dir.path(), "remove");
        assert!(!repo.tracked("lib.sh"));
        assert!(
            repo.deleted("lib.sh").is_some(),
            "the deletion should still be found by name"
        );
    }

    /// A value that happens to quote a glob (`hooks/recall-*`) is matched
    /// literally, never expanded into every file under `hooks/`.
    #[test]
    fn a_quoted_glob_is_matched_literally_not_expanded() {
        let dir = git_fixture();
        std::fs::create_dir_all(dir.path().join("hooks")).unwrap();
        std::fs::write(dir.path().join("hooks").join("recall-pull"), "x\n").unwrap();
        git_commit(dir.path(), "add");
        let repo = Repo::at(Some(dir.path().to_path_buf()));
        assert!(
            !repo.tracked("hooks/recall-*"),
            "a literal glob-looking path must not match every file under hooks/"
        );
    }

    /// Every request a review makes goes through `sources`: `/health` and
    /// discovery at the configured server, inside `status`'s collection,
    /// and a host's discovery document with `--probe-hosts`. This file
    /// opens no connection of its own. The forbidden words are built at
    /// runtime so this assertion cannot trivially match itself.
    #[test]
    fn this_module_never_touches_the_network() {
        let src = include_str!("review.rs");
        let forbidden = [
            ["req", "west"].concat(),
            ["std::", "net::"].concat(),
            ["Tcp", "Stream"].concat(),
        ];
        for word in &forbidden {
            assert!(!src.contains(word.as_str()), "found {word:?} in review.rs");
        }
    }
}
