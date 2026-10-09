//! `recall review apply` — makes one claim's suggested edit, through the
//! path `recall eval apply` takes ([`crate::edit`]).
//!
//! Only a `stale` claim `claude` was asked about (layer 3) carries a
//! suggestion, and the suggestion is always the claim rewritten as a
//! record of what changed, never a deletion (see
//! [`super::claude::rewrite_ok`]). The edit replaces the claim's own words,
//! found once in the lines it occupies, and keeps every other byte, so a
//! sentence sharing or wrapping onto its lines is untouched.
//!
//! **Built, never trusted.** The report's `suggested_edit` is for reading.
//! `apply` builds the edit again from the file as it is, the claim, and the
//! rewrite `.recall-review.json` holds for it, and holds that rewrite to the
//! rules again ([`build`]): a file anyone on this machine can edit decides
//! nothing about which bytes change. The edit is refused when:
//!
//! - the claim is a fenced block, or spans more than its words (only prose
//!   is rewritten);
//! - it holds a secret (`claude` saw it masked, so its rewrite would put the
//!   mask in the note);
//! - its words are not in its lines exactly once;
//! - another claim on those lines (a record, a claim still true, a rule,
//!   anything) cannot be found, or would lose a byte.

use recall_wire::SuggestedEdit;

use super::{claude, extract_claims, load_state, Verdict};
use crate::edit::{self, refused, Hints};
use crate::project as proj;

const COMMAND: &str = "recall review";

const HINTS: Hints = Hints {
    rerun: "recall review run --claude",
    again: "recall review apply",
};

/// The byte offset each line of `content` starts at, and one past the end.
fn line_starts(content: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(content.match_indices('\n').map(|(i, _)| i + 1));
    if *starts.last().unwrap() != content.len() {
        starts.push(content.len());
    }
    starts
}

/// The bytes of `content` that lines `lines` (1-based, inclusive) occupy.
fn block_of(content: &str, lines: [u32; 2]) -> Option<(usize, usize)> {
    let starts = line_starts(content);
    let first = lines[0].checked_sub(1)? as usize;
    let last = lines[1] as usize;
    if first >= last || last >= starts.len() {
        return None;
    }
    Some((starts[first], starts[last]))
}

/// Where `text`'s words are in `content`, within lines `lines`, whatever
/// whitespace separates them: a byte range of `content`, when they occur
/// there exactly once.
pub(super) fn span_of(content: &str, lines: [u32; 2], text: &str) -> Option<(usize, usize)> {
    let (start, end) = block_of(content, lines)?;
    let block = &content[start..end];
    let wanted: Vec<&str> = text.split_whitespace().collect();
    if wanted.is_empty() {
        return None;
    }
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut at = None;
    for (i, c) in block.char_indices() {
        match (c.is_whitespace(), at) {
            (true, Some(s)) => {
                spans.push((s, i));
                at = None;
            }
            (false, None) => at = Some(i),
            _ => {}
        }
    }
    if let Some(s) = at {
        spans.push((s, block.len()));
    }
    let mut found = (0..(spans.len() + 1).saturating_sub(wanted.len())).filter(|&i| {
        wanted
            .iter()
            .zip(&spans[i..])
            .all(|(w, &(a, b))| &block[a..b] == *w)
    });
    let i = found.next()?;
    if found.next().is_some() {
        return None;
    }
    Some((start + spans[i].0, start + spans[i + wanted.len() - 1].1))
}

/// The edit that puts `rewrite` in place of the claim at `lines` saying
/// `text`, in `content` (the file `file_path` of the scope `project_key`),
/// or why there is none. Everything the module doc lists is checked here,
/// both when the review offers an edit and again when `apply` makes it.
pub(super) fn build(
    content: &str,
    lines: [u32; 2],
    text: &str,
    (project_key, file_path): (&str, &str),
    rewrite: &str,
) -> Result<SuggestedEdit, String> {
    let fence = text.trim_start();
    if text.contains('\n') || fence.starts_with("```") || fence.starts_with("~~~") {
        return Err("it is a block, and only prose is rewritten".into());
    }
    if !claude::rewrite_ok(text, rewrite) {
        return Err("its rewrite is not a one-line record of what changed".into());
    }
    let redactor = recall_worker::Redactor::new(&[recall_wire::EvaluateFile {
        file_path: file_path.to_string(),
        content: content.to_string(),
        ..Default::default()
    }]);
    if redactor.text(text) != text {
        return Err("it holds a secret, which claude only saw masked".into());
    }
    let (from, to) =
        span_of(content, lines, text).ok_or("its words are not in its lines exactly once")?;
    for other in extract_claims(content) {
        let overlaps = other.lines[0] <= lines[1] && other.lines[1] >= lines[0];
        if !overlaps || (other.lines == lines && other.text == text) {
            continue;
        }
        match span_of(content, other.lines, &other.text) {
            Some((a, b)) if b <= from || a >= to => {}
            _ => {
                return Err(format!(
                    "it would touch another claim on its lines ({}: \"{}\")",
                    edit::lines(&other.lines),
                    edit::printable(&other.text)
                ))
            }
        }
    }
    let (start, end) = block_of(content, lines).ok_or("its lines are not in the file")?;
    Ok(SuggestedEdit {
        project_key: project_key.to_string(),
        file_path: file_path.to_string(),
        base_sha256: recall_wire::content_sha256(content),
        lines,
        replacement: format!("{}{rewrite}{}", &content[start..from], &content[to..end]),
    })
}

/// `recall review apply <id>`.
pub(super) async fn run(id: &str, yes: bool) -> anyhow::Result<i32> {
    let here = proj::resolve();
    let saved = load_state(&here.review_file());
    let Some(report) = &saved.report else {
        return Ok(edit::said(
            COMMAND,
            refused("there is no review yet.", "Make one: `recall review run`"),
        ));
    };
    let Some(claim) = report.claims.iter().find(|c| c.id == id) else {
        return Ok(edit::said(
            COMMAND,
            refused(
                format!("the last review has no claim {id}."),
                "To see them: `recall review show`",
            ),
        ));
    };
    let no_edit = || {
        let then = if claim.verdict == Some(Verdict::Stale) {
            "To ask claude for one: `recall review run --claude`"
        } else {
            ""
        };
        Ok(edit::said(
            COMMAND,
            refused(
                format!(
                    "{id} has no edit to make: only a stale claim claude was asked about \
                     carries one."
                ),
                then,
            ),
        ))
    };
    if claim.suggested_edit.is_none() {
        return no_edit();
    }
    // The rewrite as `.recall-review.json` keeps it, held to the rules again.
    let state = saved.files.get(&claim.file);
    let Some(rewrite) = state
        .and_then(|s| s.claude.as_ref())
        .and_then(|r| {
            r.decided
                .iter()
                .find(|d| d.lines == claim.lines && d.text == claim.text)
        })
        .map(claude::Decided::checked)
        .and_then(|d| d.rewrite)
    else {
        return no_edit();
    };
    let ctx = match here.hook_context() {
        Ok(ctx) => ctx,
        Err(e) => return Ok(edit::said(COMMAND, refused(format!("{e:#}"), ""))),
    };
    let Some((scope, path)) = recall_hooks::scope::route(&ctx.scopes, &claim.file) else {
        return Ok(edit::said(
            COMMAND,
            refused(
                format!("{} is not in a scope that is on here.", claim.file),
                "",
            ),
        ));
    };
    let local = super::join_relative(&ctx.memory_dir, &claim.file);
    let content = std::fs::read_to_string(&local).unwrap_or_default();
    if state.map(|s| s.content_sha256.as_str()) != Some(&recall_wire::content_sha256(&content)) {
        return Ok(edit::said(
            COMMAND,
            refused(
                format!(
                    "{} has changed since the review read it, so {id} may no longer fit.",
                    claim.file
                ),
                format!("Make a new review: {}", HINTS.rerun),
            ),
        ));
    }
    let suggested = match build(
        &content,
        claim.lines,
        &claim.text,
        (&scope.key, &path),
        &rewrite,
    ) {
        Ok(edit) => edit,
        Err(why) => {
            return Ok(edit::said(
                COMMAND,
                refused(
                    format!("{id} was not rewritten: {why}."),
                    "Edit the file yourself if it needs it.",
                ),
            ))
        }
    };
    let heading = format!("{id} stale  {}, {}", claim.file, edit::lines(&claim.lines));
    let code = edit::apply_and_push(COMMAND, id, &heading, &suggested, HINTS, yes).await;
    if code == recall_hooks::exit::OK {
        crate::ui::print_lines(
            0,
            &crate::ui::lines_of("To check it again: `recall review run`"),
        );
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT: (&str, &str) = ("acme/app", "deploy.md");
    const REWRITE: &str = "Until 2026-09 the server was at `a.example`; it is now at `b.example`.";

    #[test]
    fn only_the_claims_words_are_replaced() {
        let content = "# Deploy\n- The server is at `a.example`.\n";
        let edit = build(
            content,
            [2, 2],
            "The server is at `a.example`.",
            AT,
            REWRITE,
        )
        .unwrap();
        assert_eq!(edit.replacement, format!("- {REWRITE}\n"));
        assert_eq!(edit.lines, [2, 2]);
        assert_eq!(edit.base_sha256, recall_wire::content_sha256(content));
        assert_eq!(
            edit.apply_to(content).unwrap(),
            format!("# Deploy\n- {REWRITE}\n")
        );
    }

    /// A neighbour sentence wrapping onto the claim's line, or sharing it,
    /// keeps every byte: the reviewer's hard-wrapped paragraph.
    #[test]
    fn a_wrapped_neighbour_is_kept_not_refused() {
        let content = "The server runs Traefik on\nthe box. The server is at `a.example`. It used to\nbe at `z.example`.\n";
        let edit = build(
            content,
            [2, 2],
            "The server is at `a.example`.",
            AT,
            REWRITE,
        )
        .unwrap();
        assert_eq!(
            edit.apply_to(content).unwrap(),
            format!(
                "The server runs Traefik on\nthe box. {REWRITE} It used to\nbe at `z.example`.\n"
            )
        );
    }

    /// The design's "An edit over a `record` or `still_true` line is
    /// refused", at the level it can happen: the bytes the edit replaces
    /// would reach into another claim.
    #[test]
    fn an_edit_that_would_touch_another_claim_is_refused() {
        // The target's words, as the report holds them, run into the
        // record beside it: only a tampered or stale report says that.
        let content = "The server is at `a.example`. It used to be at `z.example`.\n";
        let err = build(
            content,
            [1, 1],
            "The server is at `a.example`. It used to",
            AT,
            REWRITE,
        )
        .unwrap_err();
        assert!(err.contains("another claim"), "{err}");
    }

    /// The reviewer's first finding: a fenced block is one claim, and is
    /// never swapped for a line of prose.
    #[test]
    fn a_fenced_block_is_never_rewritten() {
        let content = "```\nssh deploy@old # server is at `a.example`\n```\n";
        let text = "```\nssh deploy@old # server is at `a.example`\n```";
        assert!(build(content, [1, 3], text, AT, REWRITE).is_err());
        assert!(build(content, [1, 3], "``` ssh deploy@old", AT, REWRITE).is_err());
    }

    #[test]
    fn words_found_twice_or_not_at_all_are_refused() {
        let twice = "It is at `a.example`. It is at `a.example`.\n";
        assert!(build(twice, [1, 1], "It is at `a.example`.", AT, REWRITE).is_err());
        let content = "- The server is at `a.example`.\n";
        assert!(build(content, [1, 1], "Not in the file.", AT, REWRITE).is_err());
        assert!(build(
            content,
            [5, 5],
            "The server is at `a.example`.",
            AT,
            REWRITE
        )
        .is_err());
    }

    #[test]
    fn a_claim_holding_a_secret_is_never_rewritten() {
        let token = format!("ghp_{}", "a1B2c3D4e5".repeat(4));
        let text = format!("The key is {token} on `a.example`.");
        let content = format!("- {text}\n");
        let err = build(&content, [1, 1], &text, AT, REWRITE).unwrap_err();
        assert!(err.contains("secret"), "{err}");
    }
}
