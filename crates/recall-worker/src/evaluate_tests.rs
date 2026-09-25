//! The evaluation's checks, and the properties the design pins on them
//! (`docs/design/part5-plan.md`, "PR 6" under "Tests"): a report carries
//! no note text outside `details`; the checks that read files never call
//! `claude`, counted by a stand-in; the secret check finds every planted
//! token and nothing in ordinary notes; and the contradiction check runs
//! only when asked for.
//!
//! Every token here is fake and made at run time from pieces, so no
//! string in this file is one a secret scanner would stop at.

use std::path::PathBuf;

use super::*;
use recall_wire::evaluations::KINDS;

const P: &str = "acme/app";
const G: &str = "global:eko";

fn file(project_key: &str, file_path: &str, content: &str) -> EvaluateFile {
    EvaluateFile {
        project_key: project_key.to_string(),
        file_path: file_path.to_string(),
        content: content.to_string(),
        updated_at: "2026-09-20T10:00:00.000Z".to_string(),
    }
}

fn input(files: Vec<EvaluateFile>, contradictions: bool) -> EvaluateInput {
    EvaluateInput {
        evaluation_id: "eval_test".into(),
        projects: Vec::new(),
        contradictions,
        files,
    }
}

fn settings() -> Settings {
    Settings {
        now: OffsetDateTime::parse(
            "2026-09-25T10:00:00.000Z",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap(),
        stale_after: Duration::from_secs(90 * 24 * 60 * 60),
        deadline: None,
        cli_unavailable: None,
    }
}

/// A stand-in `claude` that counts every `-p` call in a file beside it and
/// answers each with `answer` as its result. `auth status` is not counted:
/// it is a local check that costs nothing.
struct FakeClaude {
    _dir: tempfile::TempDir,
    bin: String,
    calls: PathBuf,
}

impl FakeClaude {
    fn new(answer: &str) -> Self {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let calls = dir.path().join("calls");
        let envelope = dir.path().join("answer.json");
        std::fs::write(
            &envelope,
            serde_json::json!({"is_error": false, "result": answer}).to_string(),
        )
        .unwrap();
        let path = dir.path().join("claude");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "#!/bin/sh").unwrap();
        writeln!(
            f,
            r#"if [ "$1" = auth ]; then printf '%s' '{{"loggedIn":true}}'; exit 0; fi"#
        )
        .unwrap();
        writeln!(f, "echo call >> '{}'", calls.display()).unwrap();
        writeln!(f, "cat > /dev/null").unwrap();
        writeln!(f, "cat '{}'", envelope.display()).unwrap();
        drop(f);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Wait out ETXTBSY: another test's fork may still hold the write
        // handle this process just closed (see merge.rs's `settle`).
        for _ in 0..200 {
            match std::process::Command::new(&path).arg("auth").output() {
                Err(e) if e.raw_os_error() == Some(26) => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                _ => break,
            }
        }
        Self {
            bin: path.to_str().unwrap().to_string(),
            _dir: dir,
            calls,
        }
    }

    fn merger(&self) -> Merger {
        Merger::new(self.bin.clone(), Duration::from_secs(20))
    }

    fn calls(&self) -> usize {
        std::fs::read_to_string(&self.calls)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }
}

/// A fake token: `prefix` and `len` characters cycling through `alphabet`,
/// assembled here so the source holds no token-shaped string.
fn fake(prefix: &str, alphabet: &str, len: usize) -> String {
    let body: String = alphabet.chars().cycle().skip(3).take(len).collect();
    format!("{prefix}{body}")
}

const ALNUM: &str = "q7Rt2xKp9LmZ4vBn8WcJ3hYs6DfG5aEu";
const UPPER: &str = "Q7RT2XKP9LMZ4VBN8WCJ3HYS6DFG5AEU";
const HEX: &str = "9f2c4b1e7a3d8c60";

/// One of each kind of token the check knows, built from its parts.
fn planted() -> Vec<(&'static str, String)> {
    vec![
        ("AWS access key", fake(&["AK", "IA"].concat(), UPPER, 16)),
        ("AWS session key", fake(&["AS", "IA"].concat(), UPPER, 16)),
        ("GitHub token", fake(&["gh", "p_"].concat(), ALNUM, 36)),
        (
            "GitHub OAuth token",
            fake(&["gh", "o_"].concat(), ALNUM, 36),
        ),
        ("GitHub app token", fake(&["gh", "s_"].concat(), ALNUM, 36)),
        (
            "GitHub fine-grained token",
            fake(&["github", "_pat_"].concat(), "11AbCdEf_GhIjKl2", 82),
        ),
        ("GitLab token", fake(&["gl", "pat-"].concat(), ALNUM, 20)),
        (
            "Slack token",
            fake(&["xo", "xb-"].concat(), "1234-abcdEFGH", 40),
        ),
        ("Stripe key", fake(&["sk", "_live_"].concat(), ALNUM, 24)),
        (
            "Anthropic key",
            fake(&["sk", "-ant-"].concat(), "api03-AbCdEf_12", 90),
        ),
        (
            "OpenAI project key",
            fake(&["sk", "-proj-"].concat(), ALNUM, 48),
        ),
        ("OpenAI key", fake(&["sk", "-"].concat(), ALNUM, 48)),
        (
            "Google API key",
            fake(&["AI", "za"].concat(), "SyB-4x_Q9", 35),
        ),
        (
            "Recall authkey",
            fake(&["recall", "-ak-"].concat(), ALNUM, 52),
        ),
        (
            "Recall enrolment key",
            fake(&["recall", "-ek-"].concat(), "abc.DEF_12", 43),
        ),
        (
            "Recall recovery key",
            fake(&["recall", "-rk-"].concat(), "abc-DEF_12", 43),
        ),
        (
            "JSON Web Token",
            format!(
                "{}.{}.{}",
                fake(&["ey", "J"].concat(), "hbGciOiJIUzI1NiJ9", 30),
                fake("", "eyJzdWIiOiIxMjM0", 40),
                fake("", "SflKxwRJSMeKKF2QT4", 43)
            ),
        ),
        ("hex secret", fake("", HEX, 40)),
    ]
}

// ---------------------------------------------------------------------------
// the properties
// ---------------------------------------------------------------------------

/// A report made from notes holding sentinel strings, one in what each
/// check reports on, carries none of them outside `details`: the findings,
/// serialized as the server receives them, never quote a note. `details`
/// does, which shows the sentinels were where the checks looked.
#[tokio::test]
async fn a_report_carries_no_note_text_outside_details() {
    let token = fake(&["gh", "p_"].concat(), ALNUM, 36);
    let mut old = file(P, "old.md", "Deploy with `make ship SENTINEL-stale`\n");
    old.updated_at = "2025-01-01T00:00:00.000Z".into();
    let files = vec![
        file(
            P,
            "MEMORY.md",
            "- [Gone](gone-SENTINEL-link.md) SENTINEL-index\n- [Old](old.md)\n",
        ),
        file(
            P,
            "a.md",
            "- the staging database listens on 5433 SENTINEL-dup\n\nThe build SENTINEL-contra-a is green.\n",
        ),
        file(P, "b.md", "- the staging database listens on 5433 SENTINEL-dup\n"),
        file(P, "deploy.md", &format!("token for prod: {token} SENTINEL-secret\n")),
        file(
            P,
            "me.md",
            "---\nname: SENTINEL-scope\ntype: user\n---\nPrefers tabs\n",
        ),
        old,
    ];
    let claude = FakeClaude::new(
        r#"{"contradictions":[{"file":"F2","lines":[3,3],"other_file":"F3","other_lines":[1,1],"explanation":"SENTINEL-contra says otherwise"}]}"#,
    );
    let report = evaluate(&input(files, true), &settings(), &claude.merger()).await;

    let kinds: HashSet<&str> = report.findings.iter().map(|f| f.kind.as_str()).collect();
    assert_eq!(kinds, KINDS.into_iter().collect(), "{:#?}", report.findings);
    let findings = serde_json::to_string(&report.findings).unwrap();
    assert!(!findings.contains("SENTINEL"), "{findings}");
    assert!(!findings.contains(&token), "{findings}");
    let details = serde_json::to_string(&report.details).unwrap();
    for sentinel in [
        "SENTINEL-stale",
        "SENTINEL-index",
        "SENTINEL-dup",
        "SENTINEL-secret",
        "SENTINEL-scope",
        "SENTINEL-contra",
    ] {
        assert!(details.contains(sentinel), "{sentinel} is not in {details}");
    }
    // A secret is masked even in the details.
    assert!(!details.contains(&token), "{details}");
}

/// The five checks that read files never call `claude`: a stand-in counts
/// every call, and a run without the contradiction check, over notes that
/// set off all five, makes none.
#[tokio::test]
async fn the_checks_that_read_files_make_no_claude_call() {
    let claude = FakeClaude::new(r#"{"contradictions":[]}"#);
    let mut old = file(P, "old.md", "Run `./scripts/release.sh` to cut one\n");
    old.updated_at = "2024-06-01T00:00:00.000Z".into();
    let files = vec![
        file(P, "MEMORY.md", "- [Gone](gone.md)\n"),
        file(P, "a.md", "- the staging database listens on 5433\n"),
        file(G, "db.md", "- the staging database listens on 5433\n"),
        file(
            P,
            "keys.md",
            &format!("aws: {}\n", fake(&["AK", "IA"].concat(), UPPER, 16)),
        ),
        file(P, "me.md", "---\ntype: user\n---\nI like tabs\n"),
        old,
    ];
    let report = evaluate(&input(files, false), &settings(), &claude.merger()).await;
    let kinds: HashSet<&str> = report.findings.iter().map(|f| f.kind.as_str()).collect();
    assert_eq!(
        kinds,
        HashSet::from([
            KIND_SECRET,
            KIND_DUPLICATE,
            KIND_DEAD_LINK,
            KIND_WRONG_SCOPE,
            KIND_STALE
        ]),
        "{:#?}",
        report.findings
    );
    assert_eq!(claude.calls(), 0, "a check that reads files called claude");
}

/// Every planted token is found, on its own line, and a set of ordinary
/// notes, hashes, commit ids, paths and talk about tokens included, sets
/// off nothing.
#[test]
fn the_secret_check_finds_every_planted_token_and_nothing_in_ordinary_notes() {
    let planted = planted();
    let mut content = String::from("# Credentials, to move out of here\n\n");
    for (what, token) in &planted {
        content.push_str(&format!("- {what}: {token}\n"));
    }
    content.push_str("- private_key: ");
    content.push_str(&fake("", HEX, 64));
    content.push('\n');
    let key = format!(
        "{}\nMIIEpAIBAAKCAQEA0Z3VS5JJcds3xfn\nzYy8x2Cc\n{}\n",
        ["-----BEGIN RSA ", "PRIVATE KEY-----"].concat(),
        ["-----END RSA ", "PRIVATE KEY-----"].concat()
    );
    content.push_str(&key);
    let f = file(P, "creds.md", &content);
    let found = secrets(&f);
    let lines: Vec<u32> = found.iter().map(|f| f.lines[0]).collect();
    let expected: Vec<u32> = (3..=planted.len() as u32 + 3).collect();
    assert_eq!(
        &lines[..expected.len()],
        expected.as_slice(),
        "{:#?}",
        found
            .iter()
            .map(|f| (&f.lines, &f.detail.excerpt))
            .collect::<Vec<_>>()
    );
    let pem = found.last().unwrap();
    assert_eq!(
        pem.lines,
        [expected.len() as u32 + 3, expected.len() as u32 + 6]
    );
    assert_eq!(found.len(), expected.len() + 1);
    for f in &found {
        let detail = serde_json::to_string(&f.detail).unwrap();
        for (_, token) in &planted {
            assert!(!detail.contains(token.as_str()), "{token} kept in {detail}");
        }
        assert!(!detail.contains("MIIEpAIBAAKCAQEA"), "{detail}");
        let edit = f.detail.suggested_edit.as_ref().unwrap();
        let fixed = edit.apply_to(&content).unwrap();
        let line = fixed.lines().nth(f.lines[0] as usize - 1).unwrap_or("");
        assert!(tokens_in(line).is_empty(), "{line}");
    }

    let ordinary = [
        "# Deploy\n\nThe release is tagged from commit 3f9a1c7e2b8d4f6a0c5e9b1d7f3a2c8e4b6d0f1a.\n",
        "- base_sha256 is e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n",
        "- the request id looked like 550e8400-e29b-41d4-a716-446655440000\n",
        "- GitHub tokens start with ghp_ and live in 1Password, never here\n",
        "- AWS keys start with AKIA; ours are in the vault\n",
        "- token: kept in 1Password under \"prod\"\n",
        "- password = see the team vault\n",
        "- ASIANA flights are cheaper on Tuesdays\n",
        "- run task-list-refresh before sk-lint, then open https://example.com/sk-docs\n",
        "- recall-ak- keys are made with recall authkey create\n",
        "- -----BEGIN PUBLIC KEY----- blocks are fine to keep\n",
        "- the API lives at /v1/sync and the config at ~/.config/recall/config.toml\n",
        "- colors: #ff8800 and #00aaff; build 20260925.1\n",
        "- eyJ is how a JWT starts; ours are short-lived\n",
        "- the token hash is sha256 and 64 characters long\n",
    ];
    for note in ordinary {
        let found = secrets(&file(P, "note.md", note));
        assert!(found.is_empty(), "{note:?} was reported: {found:#?}");
    }
}

/// The contradiction check spends the owner's Claude usage, so it runs
/// only when asked for: not at all without it, and once per project with
/// it, the global scope read beside each.
#[tokio::test]
async fn contradictions_run_only_when_asked_for() {
    let files = vec![
        file(P, "a.md", "- deploys go out on Fridays\n"),
        file("acme/web", "a.md", "- deploys never go out on Fridays\n"),
        file(G, "tools.md", "- I deploy on Fridays\n"),
    ];
    let claude = FakeClaude::new(r#"{"contradictions":[]}"#);
    let report = evaluate(&input(files.clone(), false), &settings(), &claude.merger()).await;
    assert_eq!(claude.calls(), 0);
    assert!(report.details.skipped.is_empty());

    let report = evaluate(&input(files, true), &settings(), &claude.merger()).await;
    assert_eq!(
        claude.calls(),
        2,
        "one call per project, none for the global scope alone"
    );
    assert!(
        report.details.skipped.is_empty(),
        "{:?}",
        report.details.skipped
    );
}

// ---------------------------------------------------------------------------
// each check
// ---------------------------------------------------------------------------

#[test]
fn a_duplicate_is_reported_on_the_copy_outside_the_global_scope() {
    let files = vec![
        file(
            P,
            "a.md",
            "# Notes\n\nThe staging database is on port 5433 now.\n\nMore.\n",
        ),
        file(G, "db.md", "the staging  database is on port 5433 now\n"),
        file(
            "acme/web",
            "x.md",
            "- The staging database is on port 5433 now.\n- other\n",
        ),
        file(P, "short.md", "- see above\n"),
        file(P, "short2.md", "- see above\n"),
    ];
    let found = duplicates(&files);
    assert_eq!(found.len(), 2, "{found:#?}");
    for f in &found {
        assert_eq!(f.related, vec![file_ref(&files[1])]);
    }
    let a = found.iter().find(|f| f.file.file_path == "a.md").unwrap();
    assert_eq!(a.lines, [3, 3]);
    let edit = a.detail.suggested_edit.as_ref().unwrap();
    assert_eq!(
        edit.apply_to(&files[0].content).unwrap(),
        "# Notes\n\nMore.\n",
        "the paragraph and the gap after it"
    );
    let x = found.iter().find(|f| f.file.file_path == "x.md").unwrap();
    assert_eq!(
        x.detail
            .suggested_edit
            .as_ref()
            .unwrap()
            .apply_to(&files[2].content)
            .unwrap(),
        "- other\n"
    );
}

#[test]
fn a_link_to_a_file_that_is_not_there_is_dead() {
    let files = vec![
        file(
            P,
            "MEMORY.md",
            "- [A](a.md)\n- [Gone](gone.md)\n- [Editor](global/editor.md)\n\
             - [Lost](global/lost.md)\n- [Site](https://example.com/x.md)\n\
             - [Up](../elsewhere.md)\n- [RAM](machine/ram.md)\n- [Spaced](my%20notes.md#top)\n",
        ),
        file(P, "a.md", "a\n"),
        file(P, "my notes.md", "b\n"),
        file(G, "editor.md", "vim\n"),
    ];
    let found = dead_links(&files);
    let lines: Vec<u32> = found.iter().map(|f| f.lines[0]).collect();
    assert_eq!(lines, [2, 4], "{found:#?}");
    assert!(found.iter().all(|f| f.related.is_empty()));
    let edit = found[0].detail.suggested_edit.as_ref().unwrap();
    assert!(!edit
        .apply_to(&files[0].content)
        .unwrap()
        .contains("gone.md"));
}

#[test]
fn a_project_note_about_the_user_is_in_the_wrong_scope() {
    let user = file(
        P,
        "me.md",
        "---\nname: Me\ntype: \"user\"\n---\nLikes tabs\n",
    );
    let found = wrong_scope(&user);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].lines, [3, 3]);
    assert!(found[0].detail.reasoning.contains("recall promote me.md"));
    assert!(wrong_scope(&file(G, "me.md", &user.content)).is_empty());
    assert!(wrong_scope(&file(P, "p.md", "---\ntype: project\n---\n")).is_empty());
    assert!(
        wrong_scope(&file(P, "p.md", "type: user\n")).is_empty(),
        "not front matter"
    );
}

#[test]
fn a_file_is_stale_once_old_and_naming_a_path_or_command() {
    let mut old = file(
        P,
        "run.md",
        "# How\n\nThe build is quick.\nRun `make ship`.\n",
    );
    old.updated_at = "2026-01-01T00:00:00.000Z".into();
    let found = stale(&old, &settings());
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].lines, [4, 4]);

    let mut fresh = old.clone();
    fresh.updated_at = "2026-09-01T00:00:00.000Z".into();
    assert!(stale(&fresh, &settings()).is_empty(), "too recent");
    let mut prose = file(P, "p.md", "Nothing to run here, just a thought.\n");
    prose.updated_at = old.updated_at.clone();
    assert!(stale(&prose, &settings()).is_empty(), "names nothing");
    let mut path = file(P, "p.md", "Logs are in /var/log/recall\n");
    path.updated_at = old.updated_at.clone();
    assert_eq!(stale(&path, &settings()).len(), 1);
}

#[test]
fn a_report_numbers_its_findings_most_urgent_first() {
    let mut old = file(P, "z.md", "`x` and `y`\n");
    old.updated_at = "2020-01-01T00:00:00.000Z".into();
    let files = vec![
        old,
        file(
            P,
            "k.md",
            &format!("key: {}\n", fake(&["AK", "IA"].concat(), UPPER, 16)),
        ),
    ];
    let report = assemble(deterministic(&input(files, false), &settings()), Vec::new());
    let ids: Vec<(&str, &str)> = report
        .findings
        .iter()
        .map(|f| (f.id.as_str(), f.kind.as_str()))
        .collect();
    assert_eq!(ids, [("f1", KIND_SECRET), ("f2", KIND_STALE)]);
    assert_eq!(
        report.details.findings.keys().collect::<Vec<_>>(),
        ["f1", "f2"]
    );
    for f in &report.findings {
        recall_wire::evaluations::check_finding(&serde_json::to_value(f).unwrap()).unwrap();
    }
}

#[tokio::test]
async fn a_contradiction_is_read_from_claudes_answer_and_checked() {
    let files = vec![
        file(P, "a.md", "- deploys go out on Fridays\n- tests first\n"),
        file(G, "tools.md", "- never deploy on a Friday\n"),
        file(G, "other.md", "- one\n- two\n"),
    ];
    // Wrapped in a fence despite the prompt; one claim cites a file that
    // was not handed over, one lines past the end, one two global notes.
    let claude = FakeClaude::new(
        "```json\n{\"contradictions\":[\
         {\"file\":\"F2\",\"lines\":[1,1],\"other_file\":\"F1\",\"other_lines\":[1,1],\"explanation\":\"Fridays\"},\
         {\"file\":\"F9\",\"lines\":[1,1],\"explanation\":\"no such file\"},\
         {\"file\":\"F1\",\"lines\":[5,6],\"explanation\":\"past the end\"},\
         {\"file\":\"F2\",\"lines\":[1,1],\"other_file\":\"F3\",\"other_lines\":[1,2],\"explanation\":\"both global\"}\
         ]}\n```",
    );
    let (found, skipped) =
        contradictions(&input(files.clone(), true), &settings(), &claude.merger()).await;
    assert!(skipped.is_empty(), "{skipped:?}");
    assert_eq!(found.len(), 1, "{found:#?}");
    assert_eq!(found[0].file, file_ref(&files[0]), "on the project's side");
    assert_eq!(found[0].lines, [1, 1]);
    assert_eq!(found[0].related, vec![file_ref(&files[1])]);
    assert!(found[0].detail.excerpt.contains("never deploy"));
}

#[tokio::test]
async fn contradictions_are_skipped_and_said_so_when_they_cannot_run() {
    let files = vec![file(P, "a.md", "- one\n")];
    let claude = FakeClaude::new(r#"{"contradictions":[]}"#);

    let mut s = settings();
    s.cli_unavailable = Some("not logged in".into());
    let (_, skipped) = contradictions(&input(files.clone(), true), &s, &claude.merger()).await;
    assert!(skipped[0].reason.contains("not logged in"), "{skipped:?}");

    let mut s = settings();
    s.deadline = Some(Instant::now() + Duration::from_secs(5));
    let (_, skipped) = contradictions(&input(files.clone(), true), &s, &claude.merger()).await;
    assert!(skipped[0].reason.contains("lease"), "{skipped:?}");

    let big = vec![file(P, "a.md", &"x".repeat(MAX_PROMPT_BYTES))];
    let (_, skipped) = contradictions(&input(big, true), &settings(), &claude.merger()).await;
    assert!(skipped[0].reason.contains("bytes"), "{skipped:?}");

    let junk = FakeClaude::new("I found nothing worth mentioning.");
    let (_, skipped) = contradictions(&input(files, true), &settings(), &junk.merger()).await;
    assert!(skipped[0].reason.contains("not the JSON"), "{skipped:?}");
    assert_eq!(claude.calls(), 0);
}
