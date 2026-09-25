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

use std::io::{self, IsTerminal};
use std::path::PathBuf;

use clap::Subcommand;
use recall_hooks::client::{self, Client};
use recall_hooks::exit;
use recall_wire::evaluations::{STATE_DONE, STATE_FAILED};
use recall_wire::{Details, Evaluation, EvaluationRequest, Finding, SuggestedEdit};

use crate::devices::{admin_client, ago, day, table};
use crate::project as proj;

/// `recall eval …`.
#[derive(Subcommand)]
pub enum Cmd {
    /// Ask the worker for a report: secrets, duplicates, dead links, notes
    /// in the wrong scope, stale notes, and contradictions when asked
    Run {
        /// A project to look at, by its key; repeat for more. Every project
        /// when left out. The global scope is always read beside them
        #[arg(long = "project", value_name = "KEY")]
        projects: Vec<String>,
        /// Also look for notes that contradict each other: one claude call
        /// per project, which spends your Claude usage
        #[arg(long)]
        contradictions: bool,
        /// Machine-readable output, for scripts
        #[arg(long)]
        json: bool,
    },
    /// Every report, newest first, with how many of each finding
    List {
        /// Machine-readable output, for scripts
        #[arg(long)]
        json: bool,
    },
    /// One report: each finding, the lines it quotes, why, and the edit
    /// that would resolve it
    Show {
        /// The report's id, such as eval_7c2kq9; the newest when left out
        id: Option<String>,
        /// Machine-readable output, for scripts
        #[arg(long)]
        json: bool,
    },
    /// Make a finding's suggested edit to the local file, and push it
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
    eprintln!("recall eval: {what}");
    if !then.is_empty() {
        eprintln!("  {then}");
    }
    Failed::Said(exit::CONFIG)
}

/// Runs one `recall eval` command.
pub async fn run(cmd: Cmd) -> anyhow::Result<i32> {
    let cfg = proj::resolve().config();
    let client = match admin_client(&cfg) {
        Ok(client) => client,
        Err(why) => {
            eprintln!("recall eval: {why}");
            return Ok(exit::CONFIG);
        }
    };
    let done = match cmd {
        Cmd::Run {
            projects,
            contradictions,
            json,
        } => ask(&client, projects, contradictions, json).await,
        Cmd::List { json } => list(&client, json).await,
        Cmd::Show { id, json } => show(&client, id, json).await,
        Cmd::Apply {
            finding,
            evaluation,
            yes,
        } => apply(&client, &finding, evaluation, yes).await,
    };
    Ok(match done {
        Ok(code) | Err(Failed::Said(code)) => code,
        Err(Failed::Server(e)) => {
            eprintln!("recall eval: {}", e.reason());
            let then = match &e {
                client::Error::Status { code: 403, .. } => {
                    "This machine's device has the sync scope. Run this on an admin device, or \
                     with the server's RECALL_TOKEN set."
                }
                client::Error::Status { code: 404, .. } if e.reason() == "not found" => {
                    "The server is older than 0.4.5, which added evaluation reports."
                }
                client::Error::Transport(_) => "Check the server is up: recall doctor",
                _ => "",
            };
            if !then.is_empty() {
                eprintln!("  {then}");
            }
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
            projects,
            contradictions,
        })
        .await?;
    if json {
        return print_json(&created);
    }
    println!(
        "Asked for {}{}. The worker makes it in the background.",
        created.id,
        if contradictions {
            ", with the contradiction check (one claude call per project)"
        } else {
            ""
        }
    );
    println!("  recall eval show {}   once it is done", created.id);
    Ok(exit::OK)
}

async fn list(client: &Client, json: bool) -> Done {
    let list = client.evaluations().await?;
    if json {
        return print_json(&list);
    }
    if list.evaluations.is_empty() {
        println!("No reports yet. recall eval run asks for one.");
        return Ok(exit::OK);
    }
    let rows: Vec<[String; 5]> = list
        .evaluations
        .iter()
        .map(|e| {
            let counts = if e.counts.is_empty() && e.state == STATE_DONE {
                "nothing found".to_string()
            } else {
                e.counts
                    .iter()
                    .map(|(kind, n)| format!("{n} {kind}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            [
                e.id.clone(),
                e.state.clone(),
                ago(&e.created_at),
                if e.projects.is_empty() {
                    "every project".to_string()
                } else {
                    e.projects.join(", ")
                },
                counts,
            ]
        })
        .collect();
    table(&["ID", "STATE", "ASKED", "PROJECTS", "FOUND"], &rows);
    Ok(exit::OK)
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
                        "recall eval run asks for one; recall eval list shows them.",
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

fn indented(text: &str) -> String {
    text.lines()
        .map(|l| format!("      {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn show(client: &Client, id: Option<String>, json: bool) -> Done {
    let evaluation = fetch(client, id, false).await?;
    if json {
        return print_json(&evaluation);
    }
    println!("Report   {}", evaluation.id);
    println!(
        "State    {}{}",
        evaluation.state,
        evaluation
            .finished_at
            .as_deref()
            .map(|at| format!(", {}", day(at)))
            .unwrap_or_default()
    );
    println!(
        "Asked    {} for {}{}",
        day(&evaluation.created_at),
        if evaluation.projects.is_empty() {
            "every project".to_string()
        } else {
            evaluation.projects.join(", ")
        },
        if evaluation.contradictions {
            ", with the contradiction check"
        } else {
            ""
        }
    );
    if let Some(error) = &evaluation.error {
        println!("Error    {error}");
    }
    if evaluation.state != STATE_DONE {
        if evaluation.state != STATE_FAILED {
            println!("\nNot done yet: the worker has not reported. Ask again in a minute.");
        }
        return Ok(exit::OK);
    }
    let details = details_of(&evaluation);
    if evaluation.findings.is_empty() {
        println!("\nNothing found.");
    }
    for f in &evaluation.findings {
        print_finding(f, &details, &evaluation.id);
    }
    if !details.skipped.is_empty() {
        println!("\nNot checked:");
        for s in &details.skipped {
            let what = if s.check.is_empty() {
                String::new()
            } else {
                format!("{} ", s.check)
            };
            let project = if s.project_key.is_empty() {
                String::new()
            } else {
                format!("in {}: ", s.project_key)
            };
            println!("  {what}{project}{}", s.reason);
        }
    }
    Ok(exit::OK)
}

fn lines(f: &[u32; 2]) -> String {
    if f[0] == f[1] {
        format!("line {}", f[0])
    } else {
        format!("lines {}-{}", f[0], f[1])
    }
}

fn print_finding(f: &Finding, details: &Details, evaluation: &str) {
    println!(
        "\n{}  {} ({})  {} {}, {}",
        f.id,
        f.kind,
        f.severity,
        f.project_key,
        f.file_path,
        lines(&f.lines)
    );
    for r in &f.related {
        println!("    related: {} {}", r.project_key, r.file_path);
    }
    let Some(detail) = details.findings.get(&f.id) else {
        return;
    };
    if !detail.excerpt.is_empty() {
        println!("{}", indented(&detail.excerpt));
    }
    if !detail.reasoning.is_empty() {
        println!("    {}", detail.reasoning);
    }
    if let Some(edit) = &detail.suggested_edit {
        if edit.replacement.is_empty() {
            println!(
                "    Suggested: remove {} of {}",
                lines(&edit.lines),
                edit.file_path
            );
        } else {
            println!(
                "    Suggested: {} of {} become:",
                lines(&edit.lines),
                edit.file_path
            );
            println!("{}", indented(&edit.replacement));
        }
        println!("    recall eval apply {} --eval {evaluation}", f.id);
    }
}

/// Where the file an edit names is on this machine: under this project's
/// memory directory, in the scope whose key the edit names.
fn local_path(ctx: &recall_hooks::Context, edit: &SuggestedEdit) -> Result<PathBuf, Failed> {
    let Some(scope) = ctx.scopes.iter().find(|s| s.key == edit.project_key) else {
        let here: Vec<&str> = ctx.scopes.iter().map(|s| s.key.as_str()).collect();
        return Err(refuse(
            &format!(
                "the edit is to {} in {}, which is not synced here (this directory syncs {}).",
                edit.file_path,
                edit.project_key,
                here.join(", ")
            ),
            "Run it from a checkout of that project, or on a machine that syncs that scope.",
        ));
    };
    if recall_wire::validate_file_path(&edit.file_path).is_err() {
        return Err(refuse(
            &format!("{} is not a path Recall syncs.", edit.file_path),
            "",
        ));
    }
    let mut path = ctx.memory_dir.clone();
    if let Some(prefix) = &scope.prefix {
        path.push(prefix);
    }
    for part in edit.file_path.split('/') {
        path.push(part);
    }
    Ok(path)
}

fn confirmed(question: &str, yes: bool) -> Result<bool, Failed> {
    if yes {
        return Ok(true);
    }
    if !(io::stdin().is_terminal() && io::stderr().is_terminal()) {
        return Err(refuse(
            "needs a terminal to ask first.",
            "In a script, pass --yes.",
        ));
    }
    cliclack::confirm(question)
        .initial_value(false)
        .interact()
        .map_err(|_| Failed::Said(exit::CONFIG))
}

/// What [`make_edit`] came to.
#[derive(Debug, PartialEq, Eq)]
enum Made {
    /// The edit is in the file.
    Written,
    /// The owner said no; nothing was changed.
    Declined,
}

/// Makes `edit` to the file at `path`, once `confirm` agrees, given what the
/// edit is, as text to show.
///
/// The file is read, and the edit checked against the version the report
/// read, before asking. Asking takes as long as the owner does, and a
/// Claude Code session may edit and push the same file meanwhile; writing
/// the edit made from the first read would then replace that edit here
/// and, pushed from a base that is now the session's, everywhere. So the
/// file is read again after the answer, and the edit is written only if it
/// is still exactly what was read before, with nothing between that check
/// and the write but the write. `--yes` goes through the same check.
fn make_edit(
    path: &std::path::Path,
    edit: &SuggestedEdit,
    confirm: impl FnOnce(&str) -> Result<bool, Failed>,
) -> Result<Made, Failed> {
    let read = |path: &std::path::Path| {
        std::fs::read_to_string(path).map_err(|e| {
            refuse(
                &format!("cannot read {}: {e}", path.display()),
                "Start a Claude Code session here first, so the file is on this machine.",
            )
        })
    };
    let local = read(path)?;
    let changed = edit.apply_to(&local).map_err(|why| {
        refuse(
            &format!("{why}, here or on another machine, so the edit may no longer fit."),
            "Pull, then run a new evaluation: recall eval run",
        )
    })?;
    let shown = if edit.replacement.is_empty() {
        format!("Edit   remove {}", lines(&edit.lines))
    } else {
        format!(
            "Edit   {} become:\n{}",
            lines(&edit.lines),
            indented(&edit.replacement)
        )
    };
    if !confirm(&shown)? {
        return Ok(Made::Declined);
    }
    if read(path)? != local {
        return Err(refuse(
            &format!(
                "{} changed while you were asked, so the edit was not made: it would have \
                 replaced that change.",
                path.display()
            ),
            "Run recall eval apply again; if the report no longer fits, run a new one.",
        ));
    }
    std::fs::write(path, changed)
        .map_err(|e| refuse(&format!("cannot write {}: {e}", path.display()), ""))?;
    Ok(Made::Written)
}

async fn apply(client: &Client, finding: &str, evaluation: Option<String>, yes: bool) -> Done {
    let evaluation = fetch(client, evaluation, true).await?;
    if evaluation.state != STATE_DONE {
        return Err(refuse(
            &format!("{} has not finished.", evaluation.id),
            "recall eval show says where it is.",
        ));
    }
    let Some(f) = evaluation.findings.iter().find(|f| f.id == finding) else {
        return Err(refuse(
            &format!("{} has no finding {finding}.", evaluation.id),
            &format!("recall eval show {} lists them.", evaluation.id),
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
            &format!("recall eval show {}", evaluation.id),
        ));
    };
    let ctx = match proj::resolve().hook_context() {
        Ok(ctx) => ctx,
        Err(e) => return Err(refuse(&format!("{e:#}"), "")),
    };
    let path = local_path(&ctx, &edit)?;
    let question = format!("Make this edit to {} and push it?", edit.file_path);
    match make_edit(&path, &edit, |shown| {
        eprintln!("{} {}", f.id, f.kind);
        eprintln!("File   {}", path.display());
        eprintln!("{shown}");
        confirmed(&question, yes)
    })? {
        Made::Declined => {
            eprintln!("Nothing was changed.");
            return Ok(exit::CONFIG);
        }
        Made::Written => {}
    }
    match recall_hooks::push(&ctx, &path).await {
        Ok(_) => {
            println!("Applied {} to {} and pushed it.", f.id, path.display());
            Ok(exit::OK)
        }
        Err(e) => {
            eprintln!("recall eval: the edit is made here, but pushing it failed: {e}");
            eprintln!("  The file is changed here, and goes with the next edit made to it.");
            Ok(exit::SERVER)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit_for(content: &str) -> SuggestedEdit {
        SuggestedEdit {
            project_key: "acme/app".into(),
            file_path: "deploy.md".into(),
            base_sha256: recall_wire::content_sha256(content),
            lines: [2, 2],
            replacement: "- key: [removed]\n".into(),
        }
    }

    /// A session that edits the file while the owner is asked keeps its
    /// edit: the suggested one is not written over it.
    #[test]
    fn an_edit_made_while_asking_is_not_written_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deploy.md");
        let content = "# Deploy\n- key: abc123\n";
        std::fs::write(&path, content).unwrap();
        let theirs = "# Deploy\n- key: abc123\n- a line a session just added\n";
        let made = make_edit(&path, &edit_for(content), |_| {
            std::fs::write(&path, theirs).unwrap();
            Ok(true)
        });
        assert!(matches!(made, Err(Failed::Said(_))), "{made:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), theirs);
    }

    #[test]
    fn an_unchanged_file_gets_the_edit_and_a_no_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deploy.md");
        let content = "# Deploy\n- key: abc123\n";
        std::fs::write(&path, content).unwrap();
        assert_eq!(
            make_edit(&path, &edit_for(content), |_| Ok(false)).unwrap(),
            Made::Declined
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
        assert_eq!(
            make_edit(&path, &edit_for(content), |_| Ok(true)).unwrap(),
            Made::Written
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# Deploy\n- key: [removed]\n"
        );
    }
}
