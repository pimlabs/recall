//! `/v1/evaluations`: reports on what memory holds, made by the worker.
//!
//! A run is asked for with [`EvaluationRequest`] and becomes an
//! [`KIND_EVALUATE`](crate::jobs::KIND_EVALUATE) job, which `recall-worker`
//! claims like a merge. The claim carries the files to look at; the worker
//! runs its checks ([`KINDS`]) and posts back an
//! [`EvaluateResult`](crate::jobs::EvaluateResult): `findings` and
//! `details`.
//!
//! **The split is the point.** A [`Finding`] holds only enums, a file's
//! identity, line numbers and the files it relates to: never a word of a
//! note. The server refuses a result whose finding carries any other key
//! ([`check_finding`]), holds an `id` of any other shape, or names a file
//! it does not store, so nothing a worker writes into a finding can be note
//! text. Everything that quotes a note, the excerpt, the reasoning and a
//! suggested edit, goes in `details` ([`Details`]), which only
//! `GET /v1/evaluations/{id}` returns, and only to the operator or an admin
//! device.
//!
//! `details` is stored as plain JSON for now. The design seals it with the
//! content key (`docs/design/part5-plan.md`, PR 6), and that is still the
//! intended end state: encryption was parked on 2026-09-25, and when it
//! comes back only how `details` is stored changes, never what a finding
//! may hold.
//!
//! Nothing here changes memory. A suggested edit is applied only when the
//! owner runs `recall eval apply`, which writes it to the local file and
//! pushes it like any other edit.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::validate::{validate_file_path, validate_project_key};

/// `POST` asks for a run; `GET` lists runs, newest first. Admin only.
pub const EVALUATIONS_PATH: &str = "/v1/evaluations";

/// `GET`: one run, with its findings and details. Admin only.
pub fn evaluation_path(id: &str) -> String {
    format!("{EVALUATIONS_PATH}/{id}")
}

/// The most projects one request may name.
pub const MAX_PROJECTS: usize = 100;

/// The most findings one result may carry. A worker that finds more keeps
/// the first this many and says so in `details`.
pub const MAX_FINDINGS: usize = 1000;

/// The most files one finding may name as related.
pub const MAX_RELATED: usize = 10;

/// A file that is in memory, where it should not be: a key, a token.
pub const KIND_SECRET: &str = "secret";
/// Two notes that say opposite things. The only check that asks `claude`,
/// and it runs only when the request asks for it.
pub const KIND_CONTRADICTION: &str = "contradiction";
/// A `MEMORY.md` line linking to a file that is not there.
pub const KIND_DEAD_LINK: &str = "dead_link";
/// A project file that says it is about the user (`type: user`), which
/// belongs in the global scope: what `recall promote` is for.
pub const KIND_WRONG_SCOPE: &str = "wrong_scope";
/// The same paragraph in two files, or two scopes.
pub const KIND_DUPLICATE: &str = "duplicate";
/// A file unchanged for a long while that names a path or a command, for
/// the owner to confirm is still true.
pub const KIND_STALE: &str = "stale";

/// Every kind of finding, most urgent first: the order a report lists them
/// in.
pub const KINDS: [&str; 6] = [
    KIND_SECRET,
    KIND_CONTRADICTION,
    KIND_DEAD_LINK,
    KIND_WRONG_SCOPE,
    KIND_DUPLICATE,
    KIND_STALE,
];

/// A finding that can wait.
pub const SEVERITY_LOW: &str = "low";
/// A finding worth fixing soon.
pub const SEVERITY_MEDIUM: &str = "medium";
/// A finding to fix now: a secret.
pub const SEVERITY_HIGH: &str = "high";

/// Every severity, lowest first.
pub const SEVERITIES: [&str; 3] = [SEVERITY_LOW, SEVERITY_MEDIUM, SEVERITY_HIGH];

/// Waiting for a worker to claim it.
pub const STATE_QUEUED: &str = "queued";
/// A worker holds it.
pub const STATE_RUNNING: &str = "running";
/// Its report is in.
pub const STATE_DONE: &str = "done";
/// Out of attempts: `error` says why.
pub const STATE_FAILED: &str = "failed";

/// How a global scope's key starts (`global:eko`). An evaluation always
/// reads the global scopes beside the projects it was asked for.
pub const GLOBAL_PREFIX: &str = "global:";

/// How a machine scope's key starts (`machine:mbp`).
pub const MACHINE_PREFIX: &str = "machine:";

/// Body of `POST /v1/evaluations`. Both members may be left out: `{}` asks
/// for every project, without the contradiction check.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluationRequest {
    /// The project keys to look at; empty for every project. The global
    /// scopes are always read beside them, since a duplicate, a dead
    /// `global/` link or a contradiction may lie between a project and
    /// them.
    #[serde(default)]
    pub projects: Vec<String>,
    /// Whether to run the contradiction check, one `claude -p` per project,
    /// which spends the owner's Claude usage. Off unless asked for.
    #[serde(default)]
    pub contradictions: bool,
}

/// `POST /v1/evaluations`'s answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluationCreated {
    /// `eval_…`.
    pub id: String,
    /// [`STATE_QUEUED`].
    pub state: String,
    /// The `evaluate` job the worker will claim.
    pub job: String,
}

/// One run, as `GET /v1/evaluations` lists it: counts, never findings or
/// details.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluationSummary {
    /// `eval_…`.
    pub id: String,
    /// [`STATE_QUEUED`], [`STATE_RUNNING`], [`STATE_DONE`] or
    /// [`STATE_FAILED`].
    pub state: String,
    /// When it was asked for.
    pub created_at: String,
    /// When its report came in, or it failed; `null` until then.
    pub finished_at: Option<String>,
    /// How many findings of each kind, by kind; only kinds it found.
    #[serde(default)]
    pub counts: BTreeMap<String, u64>,
    /// The projects asked for; empty for every project.
    #[serde(default)]
    pub projects: Vec<String>,
    /// Whether the contradiction check was asked for.
    #[serde(default)]
    pub contradictions: bool,
    /// Why it failed, or the last error of an attempt that will be retried;
    /// `null` otherwise.
    #[serde(default)]
    pub error: Option<String>,
}

/// `GET /v1/evaluations`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluationList {
    /// At most 200, newest first.
    pub evaluations: Vec<EvaluationSummary>,
}

/// `GET /v1/evaluations/{id}`: one run, its findings and its details.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Evaluation {
    /// `eval_…`.
    pub id: String,
    /// As in [`EvaluationSummary::state`].
    pub state: String,
    /// When it was asked for.
    pub created_at: String,
    /// When its report came in, or it failed; `null` until then.
    pub finished_at: Option<String>,
    /// What it found: empty until it is done.
    #[serde(default)]
    pub findings: Vec<Finding>,
    /// The excerpts, the reasoning and a suggested edit per finding, as a
    /// [`Details`]; `null` until it is done, and always `null` to the admin
    /// page's passkey session, which never holds note text.
    #[serde(default)]
    pub details: Option<Value>,
    /// The projects asked for; empty for every project.
    #[serde(default)]
    pub projects: Vec<String>,
    /// Whether the contradiction check was asked for.
    #[serde(default)]
    pub contradictions: bool,
    /// As in [`EvaluationSummary::error`].
    #[serde(default)]
    pub error: Option<String>,
}

/// One finding: what kind, how bad, where. Nothing else, ever: see the
/// module docs and [`check_finding`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// `f1`, `f2`, …: unique within its report, and how `details` and
    /// `recall eval apply` name it.
    pub id: String,
    /// One of [`KINDS`].
    pub kind: String,
    /// One of [`SEVERITIES`].
    pub severity: String,
    /// The project the file belongs to.
    pub project_key: String,
    /// The file.
    pub file_path: String,
    /// The first and last line it concerns, counting from 1.
    pub lines: [u32; 2],
    /// Other files it concerns: where a duplicate's first copy is, or what
    /// a note contradicts.
    #[serde(default)]
    pub related: Vec<FileRef>,
}

/// A file, by its project and path.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FileRef {
    /// The project.
    pub project_key: String,
    /// The file.
    pub file_path: String,
}

/// The keys a finding has, in order, and the only ones it may.
pub const FINDING_KEYS: [&str; 7] = [
    "id",
    "kind",
    "severity",
    "project_key",
    "file_path",
    "lines",
    "related",
];

/// The keys a related file has, and the only ones it may.
pub const FILE_REF_KEYS: [&str; 2] = ["project_key", "file_path"];

/// Whether `id` is a finding id: `f` and a number from 1, at most six
/// digits.
pub fn is_finding_id(id: &str) -> bool {
    let Some(digits) = id.strip_prefix('f') else {
        return false;
    };
    (1..=6).contains(&digits.len())
        && digits.bytes().all(|b| b.is_ascii_digit())
        && !digits.starts_with('0')
}

/// Reads one finding as the server receives it, refusing anything but the
/// shape [`Finding`] has: exactly [`FINDING_KEYS`], an id [`is_finding_id`]
/// accepts, a kind and a severity from their lists, two line numbers from
/// 1 in order, a valid project key and path, and at most [`MAX_RELATED`]
/// related files, each exactly [`FILE_REF_KEYS`].
///
/// This is what keeps note text out of the part of a report the API can
/// read: a free-text member has nowhere to go. Whether each file named is
/// one the server holds is the server's own check, after this.
pub fn check_finding(value: &Value) -> Result<Finding, String> {
    let Some(map) = value.as_object() else {
        return Err("a finding is not an object".to_string());
    };
    let id = map
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("(no id)")
        .to_string();
    exact_keys(map, &FINDING_KEYS, &format!("finding {id}"))?;
    let finding: Finding =
        serde_json::from_value(value.clone()).map_err(|e| format!("finding {id}: {e}"))?;
    if !is_finding_id(&finding.id) {
        return Err(format!("finding {id}: an id is f and a number, such as f1"));
    }
    if !KINDS.contains(&finding.kind.as_str()) {
        return Err(format!("finding {id}: no kind {:?} exists", finding.kind));
    }
    if !SEVERITIES.contains(&finding.severity.as_str()) {
        return Err(format!(
            "finding {id}: no severity {:?} exists",
            finding.severity
        ));
    }
    let [first, last] = finding.lines;
    if first == 0 || last < first {
        return Err(format!(
            "finding {id}: lines are two line numbers from 1, the first no later than the last"
        ));
    }
    check_file(&finding.project_key, &finding.file_path)
        .map_err(|e| format!("finding {id}: {e}"))?;
    let related = map["related"]
        .as_array()
        .ok_or_else(|| format!("finding {id}: related is not a list"))?;
    if related.len() > MAX_RELATED {
        return Err(format!("finding {id}: at most {MAX_RELATED} related files"));
    }
    for r in related {
        let Some(r) = r.as_object() else {
            return Err(format!("finding {id}: a related file is not an object"));
        };
        exact_keys(r, &FILE_REF_KEYS, &format!("finding {id}'s related file"))?;
    }
    for r in &finding.related {
        check_file(&r.project_key, &r.file_path).map_err(|e| format!("finding {id}: {e}"))?;
    }
    Ok(finding)
}

fn check_file(project_key: &str, file_path: &str) -> Result<(), String> {
    validate_project_key(project_key).map_err(|e| e.to_string())?;
    validate_file_path(file_path).map_err(|e| e.to_string())?;
    Ok(())
}

fn exact_keys(
    map: &serde_json::Map<String, Value>,
    keys: &[&str],
    what: &str,
) -> Result<(), String> {
    if let Some(extra) = map.keys().find(|k| !keys.contains(&k.as_str())) {
        return Err(format!(
            "{what} has a key {extra:?}; it may have only {}",
            keys.join(", ")
        ));
    }
    if let Some(missing) = keys.iter().find(|k| !map.contains_key(**k)) {
        return Err(format!("{what} has no {missing}"));
    }
    Ok(())
}

/// What `details` holds: everything about a finding that quotes a note.
/// The server stores it as it came and never reads it; the worker writes
/// it and `recall eval` reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Details {
    /// By finding id.
    #[serde(default)]
    pub findings: BTreeMap<String, FindingDetail>,
    /// What was not checked, and why: a contradiction check the CLI could
    /// not run, a project too large for one call, findings past
    /// [`MAX_FINDINGS`].
    #[serde(default)]
    pub skipped: Vec<Skipped>,
}

/// The note text behind one finding.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindingDetail {
    /// The lines concerned, as they are in the file. A secret is masked
    /// here: a report is not another place for a key to be kept.
    #[serde(default)]
    pub excerpt: String,
    /// Why this is a finding, and what to do about it.
    #[serde(default)]
    pub reasoning: String,
    /// An edit that would resolve it, when there is one to suggest.
    #[serde(default)]
    pub suggested_edit: Option<SuggestedEdit>,
}

/// An edit `recall eval apply` can make: lines `lines` of one file,
/// replaced with `replacement`, only if the file is still the version the
/// evaluation read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuggestedEdit {
    /// The project.
    pub project_key: String,
    /// The file.
    pub file_path: String,
    /// [`content_sha256`](crate::content_sha256) of the version the
    /// evaluation read: applying refuses any other.
    pub base_sha256: String,
    /// The first and last line replaced, counting from 1.
    pub lines: [u32; 2],
    /// What they become; empty removes them. Each line of it ends with a
    /// newline, as the lines it replaces did.
    pub replacement: String,
}

impl SuggestedEdit {
    /// `content` with this edit made, or why it cannot be: the content is
    /// not the version the edit was made against, or has fewer lines.
    pub fn apply_to(&self, content: &str) -> Result<String, String> {
        if crate::content_sha256(content) != self.base_sha256 {
            return Err(format!(
                "{} has changed since the evaluation read it",
                self.file_path
            ));
        }
        let lines: Vec<&str> = content.split_inclusive('\n').collect();
        let [first, last] = self.lines;
        if first == 0 || last < first || last as usize > lines.len() {
            return Err(format!("{} has no lines {first} to {last}", self.file_path));
        }
        let mut out = String::with_capacity(content.len() + self.replacement.len());
        for line in &lines[..first as usize - 1] {
            out.push_str(line);
        }
        out.push_str(&self.replacement);
        for line in &lines[last as usize..] {
            out.push_str(line);
        }
        Ok(out)
    }
}

/// Something a run did not check.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skipped {
    /// The check, one of [`KINDS`].
    pub check: String,
    /// The project it was not run for; empty for all of them.
    #[serde(default)]
    pub project_key: String,
    /// Why.
    pub reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn finding() -> Value {
        json!({"id": "f1", "kind": "secret", "severity": "high", "project_key": "acme/app",
               "file_path": "topics/deploy.md", "lines": [12, 12], "related": []})
    }

    #[test]
    fn a_finding_of_the_documented_shape_is_read() {
        let f = check_finding(&finding()).unwrap();
        assert_eq!((f.id.as_str(), f.lines), ("f1", [12, 12]));
        assert_eq!(
            serde_json::to_value(&f).unwrap(),
            finding(),
            "the keys, in order"
        );
    }

    /// The whole point: no member a note could be written into.
    #[test]
    fn a_finding_with_any_other_key_is_refused() {
        for key in ["excerpt", "reasoning", "note", "Id"] {
            let mut f = finding();
            f[key] = json!("tokens live in 1Password");
            let err = check_finding(&f).unwrap_err();
            assert!(err.contains(&format!("{key:?}")), "{err}");
        }
        let mut f = finding();
        f["related"] = json!([{"project_key": "global:eko", "file_path": "tools.md", "why": "x"}]);
        assert!(check_finding(&f).unwrap_err().contains("\"why\""));
        let mut f = finding();
        f.as_object_mut().unwrap().remove("related");
        assert!(check_finding(&f).unwrap_err().contains("has no related"));
    }

    #[test]
    fn every_member_is_held_to_its_shape() {
        let cases: [(&str, Value); 9] = [
            ("id", json!("finding one")),
            ("id", json!("f0")),
            ("id", json!("f1234567")),
            ("kind", json!("typo")),
            ("severity", json!("urgent")),
            ("lines", json!([0, 1])),
            ("lines", json!([5, 4])),
            ("file_path", json!("../etc/passwd")),
            (
                "related",
                json!(vec![json!({"project_key": "a/b", "file_path": "c.md"}); 11]),
            ),
        ];
        for (key, value) in cases {
            let mut f = finding();
            f[key] = value.clone();
            assert!(check_finding(&f).is_err(), "{key} = {value} was accepted");
        }
        assert!(is_finding_id("f999999"));
    }

    #[test]
    fn a_request_defaults_to_every_project_without_contradictions() {
        let req: EvaluationRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(req, EvaluationRequest::default());
        assert!(!req.contradictions);
    }

    #[test]
    fn an_edit_applies_only_to_the_version_it_was_made_against() {
        let content = "# Deploy\n- token: abc\n- use make deploy\n";
        let edit = SuggestedEdit {
            project_key: "acme/app".into(),
            file_path: "deploy.md".into(),
            base_sha256: crate::content_sha256(content),
            lines: [2, 2],
            replacement: "- token: [removed]\n".into(),
        };
        assert_eq!(
            edit.apply_to(content).unwrap(),
            "# Deploy\n- token: [removed]\n- use make deploy\n"
        );
        let removal = SuggestedEdit {
            replacement: String::new(),
            lines: [2, 3],
            ..edit.clone()
        };
        assert_eq!(removal.apply_to(content).unwrap(), "# Deploy\n");
        assert!(edit.apply_to("# Deploy\n").unwrap_err().contains("changed"));
        let beyond = SuggestedEdit {
            lines: [4, 4],
            ..edit
        };
        assert!(beyond.apply_to(content).unwrap_err().contains("no lines"));
    }

    #[test]
    fn a_path() {
        assert_eq!(evaluation_path("eval_a"), "/v1/evaluations/eval_a");
    }
}
