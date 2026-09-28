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

use clap::Subcommand;
use recall_hooks::client::{self, Client};
use recall_hooks::exit;
use recall_wire::evaluations::{STATE_DONE, STATE_FAILED};
use recall_wire::{Details, Evaluation, EvaluationRequest, Finding};

use crate::devices::{admin_client, ago, day, table};
use crate::edit::{self, indented, lines};
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
    let heading = format!("{} {}", f.id, f.kind);
    Ok(edit::apply_and_push(
        "recall eval",
        &f.id,
        &heading,
        &edit,
        edit::Hints {
            rerun: "recall eval run",
            again: "recall eval apply",
        },
        yes,
    )
    .await)
}
