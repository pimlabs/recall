//! The worker's newest evaluation report, beside the claims. See
//! `docs/history/memory-truth.md`'s "Beside the worker's reports, not
//! instead of them".
//!
//! The review does not look for secrets, duplicates, dead links, wrong
//! scope or contradictions; the worker does. When this machine can read
//! reports (a device enrolled as admin, or `RECALL_TOKEN`), `run` fetches
//! the newest finished one, the same two `GET`s `recall eval show` makes,
//! and each claim whose lines a finding covers names it (`eval_7c2kq9 f4`).
//! A machine that cannot read them says so once, as a source it could not
//! read, and the review goes on.

use std::time::Duration;

use recall_hooks::client;
use recall_wire::evaluations::STATE_DONE;
use recall_wire::Finding;
use serde::{Deserialize, Serialize};

use super::Claim;

/// How long the review waits for the reports, in all.
const DEADLINE: Duration = Duration::from_secs(20);

/// A finding of the worker's newest report, as a claim it covers shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalFinding {
    /// The report, such as `eval_7c2kq9`.
    pub evaluation: String,
    /// The finding within it, such as `f4`: what `recall eval apply` takes.
    pub finding: String,
    /// What kind of finding: `secret`, `duplicate`, `dead_link`,
    /// `wrong_scope`, `stale` or `contradiction`.
    pub kind: String,
    /// How much it matters: `high`, `medium` or `low`.
    pub severity: String,
}

/// What reading the reports came to.
#[derive(Debug)]
pub(super) enum Read {
    /// No server is configured: the server source already says so.
    NoServer,
    /// This machine could not read them, and why.
    Unreadable(String),
    /// No report has finished yet.
    NoneYet,
    /// The newest finished report: its id, and its findings.
    Newest(String, Vec<Finding>),
}

/// The newest finished report, when this machine can read one.
pub(super) async fn newest(cfg: &recall_hooks::ClientConfig) -> Read {
    if cfg.url.is_empty() {
        return Read::NoServer;
    }
    let client = match crate::devices::admin_client(cfg) {
        Ok(client) => client,
        Err(why) => return Read::Unreadable(format!("reports are not readable here: {why}")),
    };
    let fetch = async {
        let list = client.evaluations().await?;
        let Some(done) = list.evaluations.into_iter().find(|e| e.state == STATE_DONE) else {
            return Ok(None);
        };
        client.evaluation(&done.id).await.map(Some)
    };
    match tokio::time::timeout(DEADLINE, fetch).await {
        Err(_) => Read::Unreadable(format!(
            "the server did not list its reports within {}s",
            DEADLINE.as_secs()
        )),
        Ok(Err(
            e @ client::Error::Status {
                code: 401 | 403, ..
            },
        )) => Read::Unreadable(format!(
            "reports need a device enrolled as admin, or the server's RECALL_TOKEN ({})",
            e.reason()
        )),
        Ok(Err(e)) => Read::Unreadable(format!("the server's reports: {}", e.reason())),
        Ok(Ok(None)) => Read::NoneYet,
        Ok(Ok(Some(evaluation))) => Read::Newest(evaluation.id, evaluation.findings),
    }
}

/// Sets each claim's `eval` to the findings of `evaluation` that cover its
/// lines in its file. `scope_of` says which scope's key and path a claim's
/// file is, as the worker names it.
pub(super) fn beside(
    claims: &mut [Claim],
    evaluation: &str,
    findings: &[Finding],
    scope_of: impl Fn(&str) -> Option<(String, String)>,
) {
    for claim in claims {
        let Some((key, path)) = scope_of(&claim.file) else {
            continue;
        };
        claim.eval = findings
            .iter()
            .filter(|f| {
                f.project_key == key
                    && f.file_path == path
                    && f.lines[0] <= claim.lines[1]
                    && f.lines[1] >= claim.lines[0]
            })
            .map(|f| EvalFinding {
                evaluation: evaluation.to_string(),
                finding: f.id.clone(),
                kind: f.kind.clone(),
                severity: f.severity.clone(),
            })
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::super::Class;
    use super::*;

    fn claim(file: &str, lines: [u32; 2]) -> Claim {
        Claim {
            id: "t1".into(),
            file: file.into(),
            lines,
            class: Class::Present,
            text: "x".into(),
            verdict: None,
            layer: None,
            evidence: Vec::new(),
            suggested_edit: None,
            eval: Vec::new(),
            dismissed: false,
        }
    }

    fn finding(id: &str, key: &str, path: &str, lines: [u32; 2]) -> Finding {
        Finding {
            id: id.into(),
            kind: "stale".into(),
            severity: "low".into(),
            project_key: key.into(),
            file_path: path.into(),
            lines,
            related: Vec::new(),
        }
    }

    /// A finding is shown beside every claim whose lines it covers, in the
    /// same scope and file, and no other; a global note is matched by its
    /// path within the global scope, as the worker names it.
    #[test]
    fn a_finding_is_beside_the_claims_it_covers() {
        let mut claims = vec![
            claim("deploy.md", [2, 3]),
            claim("deploy.md", [5, 5]),
            claim("global/editor.md", [1, 1]),
            claim("deploy.md", [1, 1]),
        ];
        let findings = [
            finding("f1", "acme/app", "deploy.md", [3, 4]),
            finding("f2", "acme/app", "other.md", [2, 3]),
            finding("f3", "global", "editor.md", [1, 1]),
            finding("f4", "acme/elsewhere", "deploy.md", [2, 2]),
        ];
        beside(&mut claims, "eval_x", &findings, |file| {
            Some(match file.strip_prefix("global/") {
                Some(path) => ("global".to_string(), path.to_string()),
                None => ("acme/app".to_string(), file.to_string()),
            })
        });
        let ids = |c: &Claim| c.eval.iter().map(|e| e.finding.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&claims[0]), ["f1"]);
        assert!(ids(&claims[1]).is_empty());
        assert_eq!(ids(&claims[2]), ["f3"]);
        assert!(ids(&claims[3]).is_empty(), "ends before the finding starts");
        assert_eq!(claims[0].eval[0].evaluation, "eval_x");
    }
}
