//! `recall eval` — ask the server's worker for a report on what memory
//! holds, read the reports, and make a finding's suggested edit.
//!
//! **How loudly it may fail:** as loudly as `recall devices`. Every one of
//! these is typed by a person, so a refusal or a server's no is an error on
//! stderr and a non-zero exit (`2` when the server said it, `1` for
//! anything decided here), never a quiet success.
//!
//! Nothing here changes memory but `apply`, and `apply` only when asked:
//! it writes one finding's suggested edit into the local file, only if the
//! file is still the version the evaluation read, and pushes it the way
//! the hook pushes any edit. The report itself never changes a note.
//!
//! The other three are admin requests, so they need a device enrolled with
//! the `admin` scope or the operator's `RECALL_TOKEN`, as `recall devices`
//! does.

use std::collections::BTreeMap;

use clap::Subcommand;
use recall_hooks::client::{self, Client};
use recall_hooks::exit;
use recall_wire::evaluations::{KINDS, STATE_DONE, STATE_FAILED};
use recall_wire::{Details, Evaluation, EvaluationRequest, FileRef, Finding};

use crate::devices::{admin_client, count, done, next, relative, server_name, table, wrap, Row};
use crate::edit::{self, lines, printable};
use crate::project as proj;
use crate::ui::{self, Tone};

/// `recall eval …`.
#[derive(Subcommand)]
pub enum Cmd {
    /// Ask the worker for a report on what memory holds
    ///
    /// It looks for secrets, duplicates, dead links, notes in the wrong
    /// scope and stale notes, and for contradictions when asked.
    #[command(verbatim_doc_comment)]
    Run {
        /// A project to look at, by its key; repeat for more
        ///
        /// Every project when left out. The global scope is always read
        /// beside them.
        #[arg(long = "project", value_name = "KEY", verbatim_doc_comment)]
        projects: Vec<String>,
        /// Also look for notes that contradict each other
        ///
        /// One claude call per project, which spends your Claude usage.
        #[arg(long)]
        contradictions: bool,
        /// Machine-readable output, for scripts
        #[arg(long)]
        json: bool,
    },
    /// List every report, newest first, with its finding counts
    List {
        /// Machine-readable output, for scripts
        #[arg(long)]
        json: bool,
    },
    /// Show a report's findings
    ///
    /// Each finding, the lines it quotes, why, and the edit that would
    /// resolve it.
    #[command(verbatim_doc_comment)]
    Show {
        /// The report's id, such as eval_7c2kq9; the newest when left out
        id: Option<String>,
        /// Machine-readable output, for scripts
        #[arg(long)]
        json: bool,
    },
    /// Make a finding's suggested edit, and push it
    ///
    /// Writes it into the local file, only if the file is still the version
    /// the report read, and pushes it the way the hook pushes any edit.
    #[command(verbatim_doc_comment)]
    Apply {
        /// The finding, such as f2
        finding: String,
        /// The report it is in; the newest finished one when left out
        #[arg(long = "eval", value_name = "ID")]
        evaluation: Option<String>,
        /// Apply without asking, for scripts
        #[arg(long, short)]
        yes: bool,
    },
}

/// Why a command stopped.
#[derive(Debug)]
enum Failed {
    /// Already said, with this exit code.
    Said(i32),
    /// The server said no, or could not be reached.
    Server(client::Error),
}

impl From<client::Error> for Failed {
    fn from(e: client::Error) -> Self {
        Failed::Server(e)
    }
}

type Done = Result<i32, Failed>;

fn refuse(what: &str, then: &str) -> Failed {
    ui::refusal("recall eval", what, then);
    Failed::Said(exit::CONFIG)
}

/// Runs one `recall eval` command.
pub async fn run(cmd: Cmd) -> anyhow::Result<i32> {
    let cfg = proj::resolve().config();
    let client = match admin_client(&cfg) {
        Ok(client) => client,
        Err(why) => {
            ui::refusal("recall eval", &why, "");
            return Ok(exit::CONFIG);
        }
    };
    let outcome = match cmd {
        Cmd::Run {
            projects,
            contradictions,
            json,
        } => ask(&client, projects, contradictions, json).await,
        Cmd::List { json } => list(&client, &server_name(&cfg.url), json).await,
        Cmd::Show { id, json } => show(&client, id, json).await,
        Cmd::Apply {
            finding,
            evaluation,
            yes,
        } => apply(&client, &finding, evaluation, yes).await,
    };
    Ok(match outcome {
        Ok(code) | Err(Failed::Said(code)) => code,
        Err(Failed::Server(e)) => {
            let then = match &e {
                client::Error::Status { code: 403, .. } => {
                    "This machine's device has the sync scope. Run this on an admin device, or \
                     with the server's RECALL_TOKEN set."
                }
                client::Error::Status { code: 404, .. } if e.reason() == "not found" => {
                    "The server is older than 0.4.5, which added evaluation reports."
                }
                client::Error::Status { code: 404, .. } => "To see the reports: `recall eval list`",
                client::Error::Status { code: 409, .. } if e.reason().contains("still") => {
                    "To see where it is: `recall eval list`"
                }
                client::Error::Transport(_) => "Check the server is up: `recall doctor`",
                _ => "",
            };
            ui::refusal("recall eval", &ui::plain(&e.reason()), then);
            match e {
                client::Error::Status { .. } => exit::SERVER,
                _ => exit::CONFIG,
            }
        }
    })
}

fn print_json<T: serde::Serialize>(value: &T) -> Done {
    match serde_json::to_string_pretty(value) {
        Ok(text) => {
            println!("{text}");
            Ok(exit::OK)
        }
        Err(e) => Err(refuse(&format!("could not write JSON: {e}"), "")),
    }
}

async fn ask(client: &Client, projects: Vec<String>, contradictions: bool, json: bool) -> Done {
    let created = client
        .request_evaluation(&EvaluationRequest {
            projects: projects.clone(),
            contradictions,
        })
        .await?;
    if json {
        return print_json(&created);
    }
    done(&format!(
        "Asked for {}: {}.",
        created.id,
        asked_for(&projects, contradictions)
    ));
    anstream::println!(
        "  {}",
        ui::dim(if contradictions {
            "The worker makes it in the background, with one claude call per project."
        } else {
            "The worker makes it in the background."
        })
    );
    next(
        &format!("recall eval show {}", created.id),
        "Once it is done:",
    );
    Ok(exit::OK)
}

/// What a report was asked to look at: `every project`, or the projects
/// named, and the contradiction check when it was asked for.
fn asked_for(projects: &[String], contradictions: bool) -> String {
    let mut what = if projects.is_empty() {
        "every project".to_string()
    } else {
        printable(&projects.join(", "))
    };
    if contradictions {
        what.push_str(", with the contradiction check");
    }
    what
}

/// How wide the projects a report looked at may run in `recall eval list`.
const PROJECTS_WIDTH: usize = 28;

/// How wide what a report found may run in `recall eval list`: `recall
/// eval show` has all of it.
const FOUND_WIDTH: usize = 40;

async fn list(client: &Client, server: &str, json: bool) -> Done {
    let list = client.evaluations().await?;
    if json {
        return print_json(&list);
    }
    ui::title("recall eval list", server);
    anstream::println!();
    if list.evaluations.is_empty() {
        anstream::println!("  No reports yet.");
        next("recall eval run", "To ask the worker for one:");
        return Ok(exit::OK);
    }

    let tone_of = |state: &str, found: usize| match state {
        STATE_FAILED => Tone::Bad,
        STATE_DONE if found > 0 => Tone::Warn,
        STATE_DONE => Tone::Good,
        _ => Tone::Quiet,
    };
    let rows: Vec<Row<5>> = list
        .evaluations
        .iter()
        .map(|e| {
            let found: u64 = e.counts.values().sum();
            let tone = tone_of(&e.state, found as usize);
            let found = match e.state.as_str() {
                STATE_DONE if found == 0 => "nothing".to_string(),
                _ => kinds(&e.counts, FOUND_WIDTH),
            };
            Row {
                tone: Some(tone),
                cells: [
                    e.id.clone(),
                    e.state.clone(),
                    relative(&e.created_at),
                    ui::clip(&asked_for(&e.projects, false), PROJECTS_WIDTH),
                    found,
                ],
                under: e.error.as_deref().map(|why| ui::clip(why, WIDTH - 6)),
            }
        })
        .collect();

    // The summary first, the worst first, as the rows are marked.
    let parts: Vec<String> = [
        (Tone::Bad, "failed"),
        (Tone::Warn, "with findings"),
        (Tone::Good, "found nothing"),
        (Tone::Quiet, "not done yet"),
    ]
    .into_iter()
    .filter_map(|(tone, what)| {
        let n = rows.iter().filter(|r| r.tone == Some(tone)).count();
        (n > 0).then(|| ui::toned(tone, &format!("{} {n} {what}", tone.mark())))
    })
    .collect();
    anstream::println!("  {}", parts.join("   "));
    anstream::println!();
    table(["ID", "STATE", "ASKED", "PROJECTS", "FOUND"], &rows);

    // Newest first, as the server lists them: the newest one worth reading.
    if let Some(e) = list
        .evaluations
        .iter()
        .find(|e| e.state == STATE_DONE && !e.counts.is_empty())
    {
        anstream::println!();
        next(
            &format!("recall eval show {}", e.id),
            "The newest report with findings:",
        );
    }
    Ok(exit::OK)
}

/// `1 secret, 2 duplicate`: how many of each kind, the most urgent kind
/// first, as a report lists them; a kind this build does not know last.
/// What does not fit in `width` is counted rather than cut in half:
/// `1 secret, 1 contradiction, +3 more`.
fn kinds(counts: &BTreeMap<String, u64>, width: usize) -> String {
    let order = |kind: &str| KINDS.iter().position(|k| *k == kind).unwrap_or(KINDS.len());
    let mut kinds: Vec<(&String, &u64)> = counts.iter().collect();
    kinds.sort_by_key(|(kind, _)| order(kind));
    let parts: Vec<String> = kinds
        .iter()
        .map(|(kind, n)| format!("{n} {}", printable(kind)))
        .collect();
    let all = parts.join(", ");
    if all.chars().count() <= width || parts.len() == 1 {
        return ui::clip(&all, width);
    }
    let mut shown = parts.len();
    loop {
        shown -= 1;
        let cut = format!(
            "{}, +{} more",
            parts[..shown].join(", "),
            parts.len() - shown
        );
        if shown <= 1 || cut.chars().count() <= width {
            return cut;
        }
    }
}

/// The report `id` names, or the newest one (the newest finished one when
/// `finished`).
async fn fetch(client: &Client, id: Option<String>, finished: bool) -> Result<Evaluation, Failed> {
    let id = match id {
        Some(id) => id,
        None => {
            let list = client.evaluations().await?;
            let newest = list
                .evaluations
                .into_iter()
                .find(|e| !finished || e.state == STATE_DONE);
            match newest {
                Some(e) => e.id,
                None => {
                    return Err(refuse(
                        if finished {
                            "no report has finished yet."
                        } else {
                            "there are no reports yet."
                        },
                        "To ask for one: `recall eval run` To see them: `recall eval list`",
                    ))
                }
            }
        }
    };
    Ok(client.evaluation(&id).await?)
}

fn details_of(evaluation: &Evaluation) -> Details {
    evaluation
        .details
        .clone()
        .and_then(|d| serde_json::from_value(d).ok())
        .unwrap_or_default()
}

/// A finding's severity, as a mark and a colour, and where it sorts: `✗`
/// high, `!` medium, `○` low.
fn severity(level: &str) -> (Tone, u8) {
    match level {
        "high" => (Tone::Bad, 0),
        "medium" => (Tone::Warn, 1),
        "low" => (Tone::Quiet, 2),
        _ => (Tone::Quiet, 3),
    }
}

/// `L2`, `L4-6`: a finding's lines, as `recall review` writes a claim's.
fn short_lines(l: &[u32; 2]) -> String {
    if l[0] == l[1] {
        format!("L{}", l[0])
    } else {
        format!("L{}-{}", l[0], l[1])
    }
}

/// How wide a report's text may run: reasons are wrapped to it, and
/// quoted lines clipped to it.
const WIDTH: usize = 100;

/// How many quoted lines of a note a finding shows before saying how many
/// more there are.
const QUOTED_LINES: usize = 8;

async fn show(client: &Client, id: Option<String>, json: bool) -> Done {
    let evaluation = fetch(client, id, false).await?;
    if json {
        return print_json(&evaluation);
    }
    let e = &evaluation;
    let asked = asked_for(&e.projects, e.contradictions);
    ui::title("recall eval show", &printable(&e.id));
    let when = match (e.state.as_str(), &e.finished_at) {
        (STATE_DONE | STATE_FAILED, Some(at)) => format!("{} {}", e.state, relative(at)),
        _ => format!("{} · asked {}", e.state, relative(&e.created_at)),
    };
    anstream::println!("{}", ui::dim(&format!("{when} · {asked}")));
    anstream::println!();

    if e.state == STATE_FAILED {
        anstream::println!(
            "  {} The worker could not make this report.",
            ui::toned(Tone::Bad, Tone::Bad.mark())
        );
        if let Some(error) = &e.error {
            for line in wrap(&printable(error), WIDTH - 4) {
                anstream::println!("    {line}");
            }
        }
        let mut again = String::from("recall eval run");
        for p in &e.projects {
            again.push_str(&format!(" --project {}", printable(p)));
        }
        if e.contradictions {
            again.push_str(" --contradictions");
        }
        next(&again, "To ask for it again:");
        return Ok(exit::OK);
    }
    if e.state != STATE_DONE {
        anstream::println!(
            "  {} Not done yet: the worker has not reported.",
            ui::toned(Tone::Quiet, Tone::Quiet.mark())
        );
        next(
            &format!("recall eval show {}", printable(&e.id)),
            "Look again in a minute:",
        );
        return Ok(exit::OK);
    }

    let details = details_of(e);
    let with_edit = |f: &Finding| {
        details
            .findings
            .get(&f.id)
            .is_some_and(|d| d.suggested_edit.is_some())
    };
    if e.findings.is_empty() {
        anstream::println!(
            "  {} Nothing found{}.",
            ui::toned(Tone::Good, Tone::Good.mark()),
            if details.skipped.is_empty() {
                ""
            } else {
                " in what was checked"
            }
        );
    } else {
        // The summary first: how bad, then how much of it can be applied.
        let mut parts: Vec<String> = ["high", "medium", "low"]
            .into_iter()
            .map(|level| {
                let n = e.findings.iter().filter(|f| f.severity == level).count();
                let tone = if n == 0 {
                    Tone::Quiet
                } else {
                    severity(level).0
                };
                ui::toned(tone, &format!("{} {n} {level}", severity(level).0.mark()))
            })
            .collect();
        // The three severities are the answer even at zero, as review's
        // verdicts are; a count of edits to apply is only a count, and a
        // zero there says nothing.
        let applicable = e.findings.iter().filter(|f| with_edit(f)).count();
        if applicable > 0 {
            parts.push(ui::dim(&format!("{applicable} with an edit to apply")));
        }
        if !details.skipped.is_empty() {
            parts.push(ui::dim(&format!(
                "{} not run",
                count(details.skipped.len(), "check", "checks")
            )));
        }
        anstream::println!("  {}", parts.join("   "));
    }

    // Grouped by project, the project with the worst finding first, and
    // each project's findings the worst first.
    let mut by_project: BTreeMap<&str, Vec<&Finding>> = BTreeMap::new();
    for f in &e.findings {
        by_project
            .entry(f.project_key.as_str())
            .or_default()
            .push(f);
    }
    let worst = |fs: &[&Finding]| fs.iter().map(|f| severity(&f.severity).1).min();
    let mut projects: Vec<(&str, Vec<&Finding>)> = by_project.into_iter().collect();
    projects.sort_by_key(|(key, fs)| (worst(fs), *key));
    let id_width = e.findings.iter().map(|f| f.id.chars().count()).max();
    let kind_width = e.findings.iter().map(|f| f.kind.chars().count()).max();
    let widths = (id_width.unwrap_or(0), kind_width.unwrap_or(0));

    let mut steps = Vec::new();
    for (project, mut findings) in projects {
        findings.sort_by_key(|f| (severity(&f.severity).1, f.file_path.clone(), f.lines[0]));
        anstream::println!();
        anstream::println!("{}", ui::bold(&printable(project)));
        for f in findings {
            print_finding(f, &details, &e.id, widths);
            if with_edit(f) {
                steps.push(apply_command(f, &e.id));
            }
        }
    }

    if !details.skipped.is_empty() {
        anstream::println!();
        anstream::println!("{}", ui::bold("Not checked"));
        for s in &details.skipped {
            let mut what = printable(&s.check);
            if !s.project_key.is_empty() {
                what.push_str(&format!(" in {}", printable(&s.project_key)));
            }
            anstream::println!(
                "  {} {}",
                ui::toned(Tone::Quiet, Tone::Quiet.mark()),
                ui::clip(&format!("{what}: {}", printable(&s.reason)), WIDTH - 4)
            );
        }
    }

    if !steps.is_empty() {
        anstream::println!();
        anstream::println!("{}", ui::bold("Next"));
        anstream::println!("  To make each finding's suggested edit:");
        for step in steps.iter().take(8) {
            ui::print_lines(2, &[ui::Line::Run(step.clone())]);
        }
        if steps.len() > 8 {
            anstream::println!("  {}", ui::dim(&format!("… and {} more", steps.len() - 8)));
        }
    }
    Ok(exit::OK)
}

/// The command that makes `f`'s suggested edit.
fn apply_command(f: &Finding, evaluation: &str) -> String {
    format!(
        "recall eval apply {} --eval {}",
        printable(&f.id),
        printable(evaluation)
    )
}

/// `acme/app search.md`, or only `search.md` within `project`.
fn file_ref(r: &FileRef, project: &str) -> String {
    if r.project_key == project {
        printable(&r.file_path)
    } else {
        printable(&format!("{} {}", r.project_key, r.file_path))
    }
}

/// One finding: its mark, id, kind, file and lines on a line, then under it
/// the files it relates to, the lines it quotes, why, and the edit that
/// would resolve it with the command that makes it. `widths` are the
/// widest id and kind in the report, so the columns line up.
fn print_finding(f: &Finding, details: &Details, evaluation: &str, widths: (usize, usize)) {
    let (tone, _) = severity(&f.severity);
    anstream::println!(
        "  {} {}  {}  {} {}",
        ui::toned(tone, tone.mark()),
        ui::bold(&format!("{:<w$}", printable(&f.id), w = widths.0)),
        ui::toned(tone, &format!("{:<w$}", printable(&f.kind), w = widths.1)),
        printable(&f.file_path),
        ui::dim(&short_lines(&f.lines))
    );
    // Under the kind, so each finding's detail reads as a block.
    let pad = " ".repeat(4 + widths.0 + 2);
    let room = WIDTH.saturating_sub(pad.len() + 2).max(40);
    if !f.related.is_empty() {
        let related: Vec<String> = f
            .related
            .iter()
            .map(|r| file_ref(r, &f.project_key))
            .collect();
        anstream::println!(
            "{pad}{}",
            ui::dim(&ui::clip(
                &format!("· related: {}", related.join(", ")),
                room
            ))
        );
    }
    let Some(detail) = details.findings.get(&f.id) else {
        return;
    };
    quote(&pad, &detail.excerpt, room, ui::dim);
    for line in wrap(&printable(&detail.reasoning), room) {
        anstream::println!("{pad}{line}");
    }
    let Some(edit) = &detail.suggested_edit else {
        return;
    };
    let file = if edit.file_path == f.file_path && edit.project_key == f.project_key {
        String::new()
    } else {
        format!(
            "{} ",
            file_ref(
                &FileRef {
                    project_key: edit.project_key.clone(),
                    file_path: edit.file_path.clone(),
                },
                &f.project_key
            )
        )
    };
    if edit.replacement.is_empty() {
        anstream::println!(
            "{pad}{}",
            ui::accent(&format!(
                "→ suggested: remove {file}{}",
                short_lines(&edit.lines)
            ))
        );
    } else {
        let become_ = if edit.replacement.lines().count() == 1 {
            "becomes"
        } else {
            "become"
        };
        anstream::println!(
            "{pad}{}",
            ui::accent(&format!(
                "→ suggested: {file}{} {become_}",
                short_lines(&edit.lines)
            ))
        );
        quote(&format!("{pad}  "), &edit.replacement, room - 2, |line| {
            ui::toned(Tone::Good, line)
        });
    }
    ui::print_lines(
        pad.chars().count(),
        &[ui::Line::Run(apply_command(f, evaluation))],
    );
}

/// `text`'s lines, each behind a `│` at `pad`, clipped to `room`, in
/// `style`: a note's own lines, set apart from what is said about them.
fn quote(pad: &str, text: &str, room: usize, style: impl Fn(&str) -> String) {
    let text = printable(text);
    let lines: Vec<&str> = text.lines().collect();
    for line in lines.iter().take(QUOTED_LINES) {
        let line = ui::clip(line, room.saturating_sub(2));
        anstream::println!("{pad}{} {}", ui::dim("│"), style(&line));
    }
    if lines.len() > QUOTED_LINES {
        anstream::println!(
            "{pad}{}",
            ui::dim(&format!(
                "│ … and {}",
                count(lines.len() - QUOTED_LINES, "more line", "more lines")
            ))
        );
    }
}

async fn apply(client: &Client, finding: &str, evaluation: Option<String>, yes: bool) -> Done {
    let evaluation = fetch(client, evaluation, true).await?;
    if evaluation.state != STATE_DONE {
        return Err(refuse(
            &format!("{} has not finished.", evaluation.id),
            &format!("To see where it is: `recall eval show {}`", evaluation.id),
        ));
    }
    let Some(f) = evaluation.findings.iter().find(|f| f.id == finding) else {
        return Err(refuse(
            &format!("{} has no finding {finding}.", evaluation.id),
            &format!("To see them: `recall eval show {}`", evaluation.id),
        ));
    };
    let details = details_of(&evaluation);
    let Some(edit) = details
        .findings
        .get(&f.id)
        .and_then(|d| d.suggested_edit.clone())
    else {
        return Err(refuse(
            &format!(
                "{} ({}) has no edit to make: its reasoning says what to do.",
                f.id, f.kind
            ),
            &format!("To read it: `recall eval show {}`", evaluation.id),
        ));
    };
    // The shape `recall review apply` gives its heading: the id and what it
    // is, then the file and its lines.
    let heading = format!("{} {}  {}, {}", f.id, f.kind, f.file_path, lines(&f.lines));
    Ok(edit::apply_and_push(
        "recall eval",
        &f.id,
        &printable(&heading),
        &edit,
        edit::Hints {
            rerun: "recall eval run",
            again: "recall eval apply",
        },
        yes,
    )
    .await)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The most urgent kind first, as a report lists them, whatever order
    /// the counts came in; one this build does not know goes last.
    #[test]
    fn kinds_are_counted_most_urgent_first() {
        let counts: BTreeMap<String, u64> = [("duplicate", 2), ("future_kind", 1), ("secret", 1)]
            .into_iter()
            .map(|(k, n)| (k.to_string(), n))
            .collect();
        assert_eq!(kinds(&counts, 80), "1 secret, 2 duplicate, 1 future_kind");
        assert_eq!(kinds(&counts, 30), "1 secret, 2 duplicate, +1 more");
        assert_eq!(kinds(&counts, 5), "1 secret, +2 more");
    }
}
