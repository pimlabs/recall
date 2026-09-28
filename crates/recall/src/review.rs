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
//! asking to review a file that is not under the memory directory.
//!
//! This PR (Part 1 of the design's plan) implements layers 1 and 2 over the
//! checkout and git only: a candidate filter for dead artefact references,
//! and evidence from `git ls-files`, `git log` and `git grep`. It reads no
//! network, no server, and no environment variable. `--claude` (layer 3),
//! `--probe-hosts` and `recall review apply` are later PRs and are simply
//! not implemented here — there is nothing in this file for them to do yet.
//!
//! `--all` is accepted and stored for forward compatibility with layer 3,
//! the only layer this design makes incremental: layers 1 and 2 are cheap
//! enough to run over every claim on every run regardless (`docs/design/
//! memory-truth.md`'s "Incremental" section), so `--all` has no visible
//! effect until a later PR adds the layer it gates.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Subcommand;
use recall_hooks::exit;
use serde::{Deserialize, Serialize};

use crate::project as proj;

/// `recall review …`.
#[derive(Subcommand)]
pub enum Cmd {
    /// Extract every checkable claim from memory and say which still hold
    Run {
        /// Only these files, relative to the memory directory (as `recall
        /// status` shows it); every file in every scope that is on, when
        /// left out
        files: Vec<String>,
        /// Re-check every file. Reserved for layer 3 (`--claude`, a later
        /// PR): layers 1 and 2 already check every file on every run
        #[arg(long)]
        all: bool,
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
pub fn run(cmd: Cmd) -> anyhow::Result<i32> {
    match cmd {
        Cmd::Run { files, all, json } => run_review(&files, all, json),
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
/// only misses a stale line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    /// A present-tense statement of state. Can go stale.
    Present,
    /// A heading, change verb, negated anchor or dated correction records
    /// what used to be true. Never stale, whatever the evidence.
    Record,
    /// A claim in a `type: user` or `type: feedback` note: the owner's own
    /// instruction. Only its anchors are checked, never its substance.
    Rule,
    /// None of the above. Only layer 3 (a later PR) can judge it.
    Unsure,
    /// A merge left an inline `[CONFLICT: ...]` marker here. Reported, not
    /// judged.
    Conflict,
}

/// What a layer decided about a claim, with the evidence that decided it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Evidence contradicts the claim.
    Stale,
    /// Evidence confirms the claim.
    StillTrue,
    /// Nothing here can decide it, or it concerns another machine.
    CantTell,
}

/// One thing read to decide a claim's verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// Where this came from: `repository`, `git` or `machine` in this PR.
    pub source: String,
    /// What was found, in a sentence.
    pub detail: String,
}

/// One claim: a list item, a sentence, or a fenced block, with the line
/// range it occupies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    /// `t1`, `t2`, … — never `f<n>`, so a claim's id cannot collide with a
    /// worker finding's when the two are shown together (a later PR).
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
    /// What layer 2 decided, when this claim reached it: `present` and
    /// `rule` claims with an anchor only. Absent for a claim nothing here
    /// judges — a record, an anchorless claim, or one whose only anchors are
    /// a kind this PR does not read (a hostname, an environment variable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    /// Which layer produced the verdict.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer: Option<u8>,
    /// What was read to decide it, one entry per anchor.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<Evidence>,
}

/// The `--json` shape. Stable from its first release, under
/// `docs/reference/releasing.md`'s Versioning rules: a later PR may add a
/// field, never remove or repurpose one of these.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    /// When this review ran, RFC 3339.
    pub reviewed_at: String,
    /// The repository's `HEAD`, short form, when the project root is a git
    /// repository.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_head: Option<String>,
    /// Every claim this review extracted, in file then line order.
    pub claims: Vec<Claim>,
}

/// What `.recall-review.json` holds: per-machine, never pushed, beside
/// `.recall-state.json`. See `claude::Env::review_file`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SavedState {
    /// Each reviewed file's content hash, as of the last run — kept for
    /// layer 3 (a later PR) to decide which files changed since then.
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
}

fn load_state(path: &Path) -> SavedState {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_state(path: &Path, state: &SavedState) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(state)?)?;
    Ok(())
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

fn run_review(file_args: &[String], _all: bool, json: bool) -> anyhow::Result<i32> {
    // `_all` is reserved for layer 3 (PR 4): layers 1 and 2 cover every
    // file on every run regardless, per the design's "Incremental" section,
    // so there is nothing for this flag to change yet.
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

    let mut candidates: Vec<String> = recall_hooks::state::list_memory_files(&memory_dir)
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

    if !file_args.is_empty() {
        let wanted: HashSet<&str> = file_args.iter().map(String::as_str).collect();
        candidates.retain(|f| wanted.contains(f.as_str()));
    }
    candidates.sort();

    let repo = Repo::at(proj::git_root());

    let mut claims = Vec::new();
    let mut file_states = BTreeMap::new();
    let mut next_id: u32 = 1;
    for rel in &candidates {
        let path = join_relative(&memory_dir, rel);
        let Ok(content) = std::fs::read_to_string(&path) else {
            // Unreadable (removed mid-run, not UTF-8, a permissions race):
            // nothing to review here, and nothing worth failing the run
            // over — the next run sees it if it is still there.
            continue;
        };
        file_states.insert(
            rel.clone(),
            FileState {
                content_sha256: recall_wire::content_sha256(&content),
            },
        );
        let note_type = front_matter_type(&content);
        for raw in extract_claims(&content) {
            let class = classify(&raw.text, raw.in_record_section, note_type.as_deref());
            let (verdict, layer, evidence) = judge(class, &raw.text, &repo);
            claims.push(Claim {
                id: format!("t{next_id}"),
                file: rel.clone(),
                lines: raw.lines,
                class,
                text: raw.text,
                verdict,
                layer,
                evidence,
            });
            next_id += 1;
        }
    }

    let report = Report {
        reviewed_at: now_rfc3339(),
        repo_head: repo.head_short(),
        claims,
    };

    let saved = SavedState {
        files: file_states,
        report: Some(report.clone()),
    };
    save_state(&review_file, &saved)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_text(&report);
    }
    Ok(exit::OK)
}

fn show_last(json: bool) -> anyhow::Result<i32> {
    let here = proj::resolve();
    let saved = load_state(&here.review_file());
    let Some(report) = saved.report else {
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
/// gets no verdict at all in this PR (see [`Class`]'s doc).
fn judge(class: Class, text: &str, repo: &Repo) -> (Option<Verdict>, Option<u8>, Vec<Evidence>) {
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
    let observed: Vec<Observed> = found.iter().map(|a| evidence_for(a, repo)).collect();
    let verdict = verdict_from(&observed);
    let evidence = observed.into_iter().map(|o| o.evidence).collect();
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
    /// or Correction.
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
    while i < lines.len() {
        let raw = lines[i];
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            i += 1;
            continue;
        }
        if trimmed.starts_with('#') {
            let heading = trimmed.trim_start_matches('#').trim();
            in_record_section = is_history_heading(heading);
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
    body.starts_with("Run `") || body.starts_with("run `")
}

/// Classifies one claim, given the note's own front-matter `type:` (if any)
/// and whether it sits under a history-like heading.
fn classify(text: &str, in_record_section: bool, note_type: Option<&str>) -> Class {
    if text.contains("[CONFLICT") {
        return Class::Conflict;
    }
    if matches!(note_type, Some("user") | Some("feedback")) {
        return Class::Rule;
    }
    if in_record_section || looks_like_record(text) {
        return Class::Record;
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
    /// A repository path, or an absolute/`~/` path on this machine.
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
    /// referent might be a name in the tracked tree (`git grep -wF`).
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

fn is_pathish(tok: &str) -> bool {
    tok.starts_with('/')
        || tok.starts_with("~/")
        || tok.starts_with("./")
        || tok.starts_with("../")
        || (tok.contains('/') && !tok.contains("://"))
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
        // whether it is still referenced anywhere in the tracked tree.
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

struct Observed {
    signal: Signal,
    evidence: Evidence,
}

fn observed(signal: Signal, source: &str, detail: String) -> Observed {
    Observed {
        signal,
        evidence: Evidence {
            source: source.to_string(),
            detail,
        },
    }
}

/// A thin wrapper over `git`, run in the project root. `root` is [`None`]
/// outside a git repository, in which case every check reads as
/// [`Signal::Unknown`] rather than running `git` at all.
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
        self.git(&["ls-files", "--", path])
            .is_some_and(|s| !s.trim().is_empty())
    }

    /// The commit and date `path` was deleted in, if git's history has one.
    fn deleted(&self, path: &str) -> Option<(String, String)> {
        let out = self.git(&[
            "log",
            "--diff-filter=D",
            "-1",
            "--format=%h %cs",
            "--",
            path,
        ])?;
        let out = out.trim();
        if out.is_empty() {
            return None;
        }
        let (hash, date) = out.split_once(' ')?;
        Some((hash.to_string(), date.to_string()))
    }

    /// Whether `term` still appears anywhere in the tracked tree.
    fn references(&self, term: &str) -> Option<bool> {
        let root = self.root.as_ref()?;
        let status = Command::new("git")
            .args(["grep", "-q", "-wF", term])
            .current_dir(root)
            .status()
            .ok()?;
        // Exit 0: found. Exit 1: ran, found nothing. Anything else (2+): the
        // search itself failed (a bad pattern, no commits yet) and says
        // nothing about whether the term is still used.
        match status.code() {
            Some(0) => Some(true),
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

fn evidence_for_path(repo: &Repo, value: &str) -> Observed {
    if value.starts_with('/') || value.starts_with("~/") {
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
            format!("`{value}` was deleted in {hash} ({date}), neither at HEAD"),
        ),
        None => observed(
            Signal::Unknown,
            "git",
            format!("`{value}` is not at HEAD and git has no record of it ever being tracked"),
        ),
    }
}

fn evidence_for_semver(repo: &Repo, value: &str) -> Observed {
    let Some(claimed) = parse_semver(value) else {
        return observed(
            Signal::Unknown,
            "git",
            format!("`{value}` is not a version this can parse"),
        );
    };
    match repo.newest_tag() {
        Some((major, minor, patch, tag)) => {
            let newest = (major, minor, patch);
            if claimed < newest {
                observed(
                    Signal::Contradicts,
                    "git",
                    format!("the newest release tag is {tag}"),
                )
            } else if claimed == newest {
                observed(
                    Signal::Confirms,
                    "git",
                    format!("matches the newest release tag {tag}"),
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

fn evidence_for_code(repo: &Repo, value: &str) -> Observed {
    match repo.references(value) {
        Some(true) => observed(
            Signal::Confirms,
            "git",
            format!("`{value}` is still referenced in the tracked tree (git grep -wF)"),
        ),
        Some(false) => observed(
            Signal::Contradicts,
            "git",
            format!("`{value}` is no longer referenced anywhere in the tracked tree"),
        ),
        None => observed(
            Signal::Unknown,
            "git",
            "this is not a git repository, so it cannot be searched".to_string(),
        ),
    }
}

fn evidence_for(anchor: &Anchor, repo: &Repo) -> Observed {
    match anchor.kind {
        AnchorKind::Path => evidence_for_path(repo, &anchor.value),
        AnchorKind::SemVer => evidence_for_semver(repo, &anchor.value),
        AnchorKind::Code => evidence_for_code(repo, &anchor.value),
        AnchorKind::Hostname | AnchorKind::EnvVar => observed(
            Signal::Unknown,
            "(not checked)",
            "needs the server or the environment; a later release reads those".to_string(),
        ),
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

fn print_text(rep: &Report) {
    crate::ui::title("recall review", rep.repo_head.as_deref().unwrap_or(""));
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
                c.text
            );
            conflicts += 1;
        }
        for verdict in [Verdict::Stale, Verdict::CantTell, Verdict::StillTrue] {
            for c in claims.iter().filter(|c| c.verdict == Some(verdict)) {
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
                println!("  {:<10} {:<7} {}", tag, lines_desc(&c.lines), c.text);
                for e in &c.evidence {
                    println!("             {}", e.detail);
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

    #[test]
    fn a_history_heading_makes_every_claim_under_it_a_record() {
        let content = "It is at `x`.\n\n## History\n\nIt used to be at `y`.\n";
        let claims = claims_of(content);
        assert_eq!(claims.len(), 2);
        assert_eq!(claims[0].0, Class::Present, "{:?}", claims[0]);
        assert_eq!(claims[1].0, Class::Record, "{:?}", claims[1]);
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

    #[test]
    fn a_hostname_anchor_is_never_checked_by_this_pr() {
        let repo = Repo::at(None);
        let o = evidence_for(
            &Anchor {
                kind: AnchorKind::Hostname,
                value: "recall.pimlabs.id".into(),
            },
            &repo,
        );
        assert_eq!(o.signal, Signal::Unknown);
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
        );
        assert_eq!(verdict, None);
        assert_eq!(layer, None);
        assert!(evidence.is_empty());
    }

    #[test]
    fn an_unsure_claim_gets_no_verdict_in_this_pr() {
        let repo = Repo::at(None);
        let (verdict, layer, _) = judge(Class::Unsure, "`lib.sh` is mentioned here.", &repo);
        assert_eq!(verdict, None);
        assert_eq!(layer, None);
    }

    #[test]
    fn semver_evidence_compares_against_the_newest_tag() {
        let dir = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["commit", "--allow-empty", "-q", "-m", "x"],
            vec!["tag", "v0.4.8"],
        ] {
            assert!(Command::new("git")
                .args(&args)
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success());
        }
        let repo = Repo::at(Some(dir.path().to_path_buf()));
        let older = evidence_for_semver(&repo, "0.4.7");
        assert_eq!(
            older.signal,
            Signal::Contradicts,
            "{}",
            older.evidence.detail
        );
        let current = evidence_for_semver(&repo, "0.4.8");
        assert_eq!(
            current.signal,
            Signal::Confirms,
            "{}",
            current.evidence.detail
        );
    }
}
