//! `recall review apply` — makes one claim's suggested edit, through the
//! path `recall eval apply` takes ([`crate::edit`]).
//!
//! Only a `stale` claim `claude` was asked about (layer 3) carries a
//! suggestion, and the suggestion is always the claim rewritten as a
//! record of what changed, never a deletion (see
//! [`super::claude::rewrite_ok`]). The edit replaces the claim's own words
//! in the lines it occupies and keeps every other word on those lines, so a
//! sentence sharing a line with it is untouched.
//!
//! Before any of [`crate::edit`]'s checks, the edit is refused if its lines
//! hold another claim the replacement does not keep word for word: a
//! record, a claim still true, a rule, anything. The design names records
//! and `still_true` claims; nothing else on those lines is this edit's to
//! change either.

use recall_hooks::exit;
use recall_wire::SuggestedEdit;

use super::{load_state, Claim, Verdict};
use crate::edit::{self, refused, Hints};
use crate::project as proj;

const COMMAND: &str = "recall review";

const HINTS: Hints = Hints {
    rerun: "recall review run --claude",
    again: "recall review apply",
};

/// `text`'s words, whatever whitespace separates them.
fn words(text: &str) -> Vec<&str> {
    text.split_whitespace().collect()
}

/// The lines `lines` of `content` with `claim`'s words replaced by
/// `rewrite`, every other byte kept: [`None`] when the claim's words are
/// not found there in order (the report and the file disagree).
pub(super) fn substitute(
    content: &str,
    lines: [u32; 2],
    claim: &str,
    rewrite: &str,
) -> Option<String> {
    let all: Vec<&str> = content.split_inclusive('\n').collect();
    let block: String = all
        .get(lines[0].checked_sub(1)? as usize..lines[1] as usize)?
        .concat();
    let wanted = words(claim);
    if wanted.is_empty() {
        return None;
    }
    // Every run of non-whitespace in the block, with where it is.
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut start = None;
    for (i, c) in block.char_indices() {
        match (c.is_whitespace(), start) {
            (true, Some(s)) => {
                spans.push((s, i));
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
    }
    if let Some(s) = start {
        spans.push((s, block.len()));
    }
    let at = (0..spans.len().checked_sub(wanted.len() - 1)?).find(|&i| {
        wanted
            .iter()
            .zip(&spans[i..])
            .all(|(w, &(a, b))| &block[a..b] == *w)
    })?;
    let (from, to) = (spans[at].0, spans[at + wanted.len() - 1].1);
    Some(format!("{}{rewrite}{}", &block[..from], &block[to..]))
}

/// The edit that puts `rewrite` in place of `claim` in `content` (the file
/// `file_path` in the scope `project_key`), when the claim is found there.
pub(super) fn edit_for(
    claim: &Claim,
    content: &str,
    project_key: &str,
    file_path: &str,
    rewrite: &str,
) -> Option<SuggestedEdit> {
    Some(SuggestedEdit {
        project_key: project_key.to_string(),
        file_path: file_path.to_string(),
        base_sha256: recall_wire::content_sha256(content),
        lines: claim.lines,
        replacement: substitute(content, claim.lines, &claim.text, rewrite)?,
    })
}

/// Why `edit`, made for `target`, may not be made: another claim of the
/// same file on the lines it replaces that the replacement does not keep
/// word for word. `claims` is the report the edit came from.
pub(super) fn guard(claims: &[Claim], target: &Claim, edit: &SuggestedEdit) -> Result<(), String> {
    let kept = words(&edit.replacement).join(" ");
    let [first, last] = edit.lines;
    for c in claims.iter().filter(|c| {
        c.file == target.file && c.id != target.id && c.lines[0] <= last && c.lines[1] >= first
    }) {
        if !kept.contains(&words(&c.text).join(" ")) {
            let what = match (c.class, c.verdict) {
                (super::Class::Record, _) => "a record",
                (_, Some(Verdict::StillTrue)) => "a claim still true",
                (super::Class::Rule, _) => "a rule",
                _ => "another claim",
            };
            return Err(format!(
                "the edit to {} would change {what} it does not rewrite ({}, {})",
                target.file,
                c.id,
                edit::lines(&c.lines)
            ));
        }
    }
    if edit.replacement.trim().is_empty() {
        return Err("the edit would delete the claim; a review only rewrites".to_string());
    }
    Ok(())
}

/// `recall review apply <id>`.
pub(super) async fn run(id: &str, yes: bool) -> anyhow::Result<i32> {
    let here = proj::resolve();
    let Some(report) = load_state(&here.review_file()).report else {
        return Ok(edit::said(
            COMMAND,
            refused("there is no review yet.", "recall review run makes one."),
        ));
    };
    let Some(claim) = report.claims.iter().find(|c| c.id == id) else {
        return Ok(edit::said(
            COMMAND,
            refused(
                format!("the last review has no claim {id}."),
                "recall review show lists them.",
            ),
        ));
    };
    let Some(suggested) = &claim.suggested_edit else {
        let then = if claim.verdict == Some(Verdict::Stale) {
            "recall review run --claude asks for one."
        } else {
            ""
        };
        return Ok(edit::said(
            COMMAND,
            refused(
                format!(
                    "{id} has no edit to make: only a stale claim claude was asked about \
                     carries one."
                ),
                then,
            ),
        ));
    };
    if let Err(why) = guard(&report.claims, claim, suggested) {
        return Ok(edit::said(
            COMMAND,
            refused(
                format!("{why}, so it was not made."),
                "Edit the file yourself, or run recall review run --claude --all for a new one.",
            ),
        ));
    }
    let heading = format!("{id} stale  {}, {}", claim.file, edit::lines(&claim.lines));
    let code = edit::apply_and_push(COMMAND, id, &heading, suggested, HINTS, yes).await;
    if code == exit::OK {
        println!("recall review run checks it again.");
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::super::{Class, Evidence};
    use super::*;

    fn claim(
        id: &str,
        lines: [u32; 2],
        class: Class,
        verdict: Option<Verdict>,
        text: &str,
    ) -> Claim {
        Claim {
            id: id.into(),
            file: "deploy.md".into(),
            lines,
            class,
            text: text.into(),
            verdict,
            layer: Some(3),
            evidence: Vec::<Evidence>::new(),
            suggested_edit: None,
        }
    }

    #[test]
    fn only_the_claims_words_are_replaced() {
        let content = "# Deploy\n- The server is at `a.example`.\nIt runs Traefik. The server\nlives on homer.\n";
        assert_eq!(
            substitute(
                content,
                [2, 2],
                "The server is at `a.example`.",
                "Until 2026-09 the server was at `a.example`; it is now at `b.example`."
            )
            .unwrap(),
            "- Until 2026-09 the server was at `a.example`; it is now at `b.example`.\n"
        );
        // A sentence wrapped over two lines, beside another on the first.
        assert_eq!(
            substitute(content, [3, 4], "The server lives on homer.", "X.").unwrap(),
            "It runs Traefik. X.\n"
        );
        assert!(substitute(content, [3, 4], "Not in the file.", "X.").is_none());
        assert!(substitute(content, [9, 9], "Anything.", "X.").is_none());
    }

    /// The design's "An edit over a `record` or `still_true` line is
    /// refused": a replacement that drops another claim on its lines.
    #[test]
    fn an_edit_over_a_record_or_a_true_claim_is_refused() {
        let target = claim(
            "t2",
            [3, 3],
            Class::Present,
            Some(Verdict::Stale),
            "It is at A.",
        );
        for other in [
            claim("t3", [3, 3], Class::Record, None, "It used to be at Z."),
            claim(
                "t3",
                [3, 4],
                Class::Present,
                Some(Verdict::StillTrue),
                "It runs Traefik.",
            ),
        ] {
            let claims = vec![target.clone(), other.clone()];
            let content = "a\nb\nIt is at A. It used to be at Z.\nIt runs Traefik.\n";
            let dropped = SuggestedEdit {
                file_path: "deploy.md".into(),
                base_sha256: recall_wire::content_sha256(content),
                lines: [3, 4],
                replacement: "Until 2026 it was at A; it is now at B.\n".into(),
                ..Default::default()
            };
            let err = guard(&claims, &target, &dropped).unwrap_err();
            assert!(err.contains("t3"), "{err}");
            let kept = SuggestedEdit {
                replacement: format!("Until 2026 it was at A; it is now at B. {}\n", other.text),
                ..dropped
            };
            assert_eq!(guard(&claims, &target, &kept), Ok(()), "{other:?}");
        }
    }

    #[test]
    fn a_claim_on_other_lines_or_in_another_file_is_not_in_the_way() {
        let target = claim(
            "t1",
            [2, 2],
            Class::Present,
            Some(Verdict::Stale),
            "It is at A.",
        );
        let mut elsewhere = claim("t2", [2, 2], Class::Record, None, "It used to be at Z.");
        elsewhere.file = "other.md".into();
        let below = claim("t3", [3, 3], Class::Record, None, "It used to be at Y.");
        let edit = SuggestedEdit {
            lines: [2, 2],
            replacement: "Until 2026 it was at A; it is now at B.\n".into(),
            ..Default::default()
        };
        assert_eq!(
            guard(&[target.clone(), elsewhere, below], &target, &edit),
            Ok(())
        );
        let empty = SuggestedEdit {
            replacement: "\n".into(),
            ..edit
        };
        assert!(guard(std::slice::from_ref(&target), &target, &empty).is_err());
    }
}
