//! The witness against a fake server that keeps a real Merkle log, which a
//! test can grow, rewrite or cut back the way a server that rewrote its
//! history would.

use super::*;
use crate::testserver::FakeServer;

/// A deadline no test here meets unless something is wrong.
const LONG: Duration = Duration::from_secs(30);

struct Setup {
    server: FakeServer,
    _dir: tempfile::TempDir,
    witness: Witness,
    client: Client,
}

async fn setup(leaves: usize) -> Setup {
    let server = FakeServer::start().await;
    server.keep_audit_log(leaves);
    let dir = tempfile::tempdir().unwrap();
    let witness = Witness::new(dir.path().join(AUDIT_FILE), &server.url);
    let client = Client::new(&server.url, "token")
        .unwrap()
        .with_witness(witness.clone());
    Setup {
        server,
        _dir: dir,
        witness,
        client,
    }
}

fn cp(size: u64, byte: u8) -> Checkpoint {
    Checkpoint {
        size,
        root: [byte; 32],
    }
}

/// The finding an outcome holds, or a panic naming the outcome.
#[track_caller]
fn finding_of(found: &Witnessed) -> &Inconsistency {
    match found {
        Witnessed::Inconsistent { finding, .. } => finding,
        other => panic!("the log passed: {other:?}"),
    }
}

#[test]
fn a_checkpoint_is_kept_as_the_three_lines_of_a_c2sp_note() {
    let c = cp(1042, 7);
    let note = c.note("recall.example.com");
    assert_eq!(
        note,
        format!("recall.example.com\n1042\n{}\n", STANDARD.encode([7u8; 32]))
    );
    assert_eq!(Checkpoint::from_note(&note, "recall.example.com"), Some(c));
    assert_eq!(
        Checkpoint::from_note(&note, "elsewhere.example.com"),
        None,
        "another server's"
    );
    // A signed note, its signature line after a blank one: not what is
    // kept, and not read as if it were.
    assert_eq!(
        Checkpoint::from_note(
            &format!("{note}\n\u{2014} recall.example.com c2ln\n"),
            "recall.example.com"
        ),
        None
    );
    assert_eq!(
        Checkpoint::from_note(note.trim_end(), "recall.example.com"),
        None
    );
    let padded = note.replace("\n1042\n", "\n01042\n");
    assert_eq!(
        Checkpoint::from_note(&padded, "recall.example.com"),
        None,
        "one spelling"
    );
    assert_eq!(Checkpoint::from_header(&c.header()), Some(c));
    assert_eq!(Checkpoint::from_header("12 notbase64"), None);
}

#[test]
fn the_origin_is_the_servers_address_without_its_scheme() {
    assert_eq!(origin("https://Recall.Example.com/"), "recall.example.com");
    assert_eq!(origin("http://127.0.0.1:8932"), "127.0.0.1:8932");
    assert_eq!(origin("https://example.com/recall/"), "example.com/recall");
}

/// Every pull saves the checkpoint it carries, unchecked, and says nothing
/// about it to anyone: a hook's pull is not held up.
#[tokio::test]
async fn a_pull_saves_the_checkpoint_it_carries() {
    let s = setup(3).await;
    s.client.pull("acme/app").await.unwrap();
    let saved = s.witness.load().unwrap();
    assert_eq!(saved.checkpoints, vec![]);
    let want = Checkpoint::from_wire(&s.server.audit_checkpoint()).unwrap();
    assert_eq!(saved.unchecked, vec![want]);
    assert_eq!(want.size, 4, "the pull's own leaf");
    assert!(saved.unchecked_since.is_some(), "and when it began to wait");

    let text = std::fs::read_to_string(s.witness.file()).unwrap();
    let file: serde_json::Value = serde_json::from_str(&text).unwrap();
    let url = home::normalize_url(&s.server.url);
    assert_eq!(file["version"], 1);
    assert_eq!(
        file["servers"][&url]["unchecked"][0],
        want.note(&origin(&s.server.url))
    );
}

/// With nothing saved, the log as it is now is the first checkpoint: taken
/// on trust, since every log is first seen some time.
#[tokio::test]
async fn the_first_check_takes_the_log_as_it_is() {
    let s = setup(5).await;
    let found = s.witness.check(&s.client, LONG).await.unwrap();
    let now = Checkpoint::from_wire(&s.server.audit_checkpoint()).unwrap();
    assert_eq!(
        found,
        Witnessed::Extends {
            current: now,
            proved: 0
        }
    );
    assert_eq!(s.witness.load().unwrap().checkpoints, vec![now]);
}

/// Each saved checkpoint is proven against the log now, and moves to the
/// checked ones; the log now becomes the newest.
#[tokio::test]
async fn a_check_proves_every_saved_checkpoint() {
    let s = setup(3).await;
    s.client.pull("acme/app").await.unwrap();
    s.server.grow_audit_log(4);
    s.client.pull("acme/app").await.unwrap();
    s.server.grow_audit_log(9);
    assert_eq!(s.witness.load().unwrap().unchecked.len(), 2);

    let found = s.witness.check(&s.client, LONG).await.unwrap();
    let now = Checkpoint::from_wire(&s.server.audit_checkpoint()).unwrap();
    assert_eq!(
        found,
        Witnessed::Extends {
            current: now,
            proved: 2
        }
    );
    let saved = s.witness.load().unwrap();
    assert!(saved.unchecked.is_empty(), "{saved:?}");
    assert_eq!(saved.unchecked_since, None, "nothing waits");
    assert_eq!(saved.checkpoints.len(), 3);
    assert_eq!(saved.newest(), Some(now));

    // And again later: only the newest checked one needs proving.
    s.server.grow_audit_log(2);
    let found = s.witness.check(&s.client, LONG).await.unwrap();
    assert!(
        matches!(found, Witnessed::Extends { proved: 1, .. }),
        "{found:?}"
    );
}

/// The plan's point: history rewritten before a saved checkpoint is found,
/// written down, and stays found. Mutation: accept a proof without
/// verifying it, or verify only the new root; either makes this pass the
/// rewrite.
#[tokio::test]
async fn a_rewritten_log_is_found_and_the_finding_stays() {
    let s = setup(6).await;
    s.client.pull("acme/app").await.unwrap();
    s.witness.check(&s.client, LONG).await.unwrap();
    let before = s.witness.load().unwrap().newest().unwrap();

    s.server.rewrite_audit_leaf(2);
    s.server.grow_audit_log(3);
    // The pull after the rewrite saves what it is shown; nothing yet says
    // the log was rewritten, since no proof has been asked for.
    s.client.pull("acme/app").await.unwrap();

    let found = s.witness.check(&s.client, LONG).await.unwrap();
    let finding = finding_of(&found).clone();
    assert!(finding.detail.contains("does not verify"), "{finding:?}");
    assert_eq!(finding.saved, before.note(s.witness.origin()));
    assert!(matches!(
        found,
        Witnessed::Inconsistent { unsaved: None, .. }
    ));

    // Written down, and every checkpoint kept beside it as evidence.
    let saved = s.witness.load().unwrap();
    assert_eq!(saved.inconsistent.as_ref(), Some(&finding));
    assert_eq!(
        saved.newest(),
        Some(before),
        "nothing newer was taken as the truth"
    );

    // It stays: a later check answers it without asking the server, and a
    // later pull does not clear it.
    s.server.fail_with(500, r#"{"error":"down"}"#);
    let again = s.witness.check(&s.client, LONG).await.unwrap();
    assert_eq!(
        again,
        Witnessed::Inconsistent {
            finding: finding.clone(),
            unsaved: None
        }
    );

    // Until it is reset, which forgets this server and nothing else.
    let held = s.witness.reset().unwrap();
    assert_eq!(held.inconsistent, Some(finding));
    assert!(s.witness.load().unwrap().is_empty());
}

/// The review's case against thinning what waits: honest checkpoints up to
/// size 9, then leaf 6 rewritten, then many more pulls. Only the
/// checkpoints at 7 to 9 cover the rewritten leaf and predate the rewrite;
/// keeping "the smallest and the newest" dropped exactly those, and the
/// check then passed the rewrite. Mutation: keep 32 unchecked, as before.
#[tokio::test]
async fn a_rewrite_between_many_waiting_checkpoints_is_found() {
    let s = setup(5).await;
    for _ in 0..4 {
        s.client.pull("acme/app").await.unwrap();
    }
    assert_eq!(s.server.audit_checkpoint().tree_size, 9);
    s.server.rewrite_audit_leaf(6);
    for _ in 0..40 {
        s.client.pull("acme/app").await.unwrap();
    }
    let saved = s.witness.load().unwrap();
    assert_eq!(saved.unchecked.len(), 44, "every one kept");
    assert_eq!(saved.dropped, 0);
    let found = s.witness.check(&s.client, LONG).await.unwrap();
    let finding = finding_of(&found);
    assert!(finding.saved.contains("\n7\n"), "{finding:?}");
}

/// Past the bound, what is dropped is counted, so the gap is reported and
/// never looks like a clean record; a reset forgets the count too.
#[test]
fn what_is_dropped_past_the_bound_is_counted() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Witness::new(dir.path().join(AUDIT_FILE), "https://r.example");
    w.keep = 8;
    for size in 1..=12 {
        w.record(&cp(size, size as u8).header()).unwrap();
    }
    let saved = w.load().unwrap();
    assert_eq!(saved.unchecked.len(), 8);
    assert_eq!(saved.unchecked[0], cp(1, 1), "the smallest stays");
    assert_eq!(saved.unchecked.last(), Some(&cp(12, 12)), "and the newest");
    assert_eq!(saved.dropped, 4);
    assert!(!saved.is_empty());
    w.reset().unwrap();
    assert_eq!(w.load().unwrap().dropped, 0);
}

/// The real bound is thousands, not dozens: a year and more of session
/// starts on a machine that never runs a check.
#[test]
fn the_bound_on_what_waits_is_generous() {
    let w = Witness::new("audit.json", "https://r.example");
    assert!(w.keep >= 4096, "{}", w.keep);
    const { assert!(NUDGE_AFTER < 32, "the nudge comes long before") };
}

/// Each proof is kept the moment it verifies: a check the server's rate
/// limit cuts short keeps what it proved, and the next carries on from
/// there. Mutation: write everything proven down only once all are.
#[tokio::test]
async fn a_check_cut_short_keeps_what_it_proved() {
    let s = setup(3).await;
    for _ in 0..5 {
        s.client.pull("acme/app").await.unwrap();
        s.server.grow_audit_log(1);
    }
    assert_eq!(s.witness.load().unwrap().unchecked.len(), 5);
    s.server.limit_proofs_after(Some(2));
    let err = s
        .witness
        .check(&s.client, Duration::from_millis(500))
        .await
        .unwrap_err();
    assert!(err.unanswered(), "{err:?}");
    let saved = s.witness.load().unwrap();
    assert_eq!(saved.unchecked.len(), 3, "two proven and kept: {saved:?}");
    assert_eq!(saved.checkpoints.len(), 2);
    assert!(
        saved.unchecked[0].size > saved.checkpoints[1].size,
        "smallest first"
    );

    s.server.limit_proofs_after(None);
    let found = s.witness.check(&s.client, LONG).await.unwrap();
    assert!(matches!(found, Witnessed::Extends { .. }), "{found:?}");
    assert!(s.witness.load().unwrap().unchecked.is_empty());
}

/// A request the rate limit refuses is asked again, and one that frees up
/// within the deadline completes the check. Mutation: give up at the first
/// 429.
#[tokio::test]
async fn a_rate_limited_proof_is_asked_again() {
    let s = setup(3).await;
    s.client.pull("acme/app").await.unwrap();
    s.server.grow_audit_log(2);
    s.server.limit_proofs_after(Some(0));
    let check = s.witness.check(&s.client, LONG);
    let lift = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        s.server.limit_proofs_after(None);
    };
    let (found, ()) = tokio::join!(check, lift);
    assert!(
        matches!(found, Ok(Witnessed::Extends { proved: 1, .. })),
        "{found:?}"
    );
}

/// A server too slow to answer does not hold the check past its deadline,
/// which is what bounds `recall doctor` against it. Mutation: no deadline.
#[tokio::test]
async fn a_check_stops_at_its_deadline() {
    let s = setup(3).await;
    s.client.pull("acme/app").await.unwrap();
    s.server.grow_audit_log(2);
    s.server.hang_proofs();
    let started = std::time::Instant::now();
    let err = s
        .witness
        .check(&s.client, Duration::from_millis(300))
        .await
        .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(matches!(err, CheckError::Deadline(_)) && err.unanswered());
}

/// How many checks in a row went unanswered is kept, so a server that
/// never answers can be failed on; any answer resets it. Mutation: never
/// count, or never reset.
#[tokio::test]
async fn unanswered_checks_in_a_row_are_counted() {
    let s = setup(3).await;
    s.client.pull("acme/app").await.unwrap();
    s.server.fail_with(429, r#"{"error":"too many requests"}"#);
    for want in 1..=3 {
        s.witness
            .check(&s.client, Duration::from_millis(50))
            .await
            .unwrap_err();
        assert_eq!(s.witness.load().unwrap().unanswered, want);
    }
    s.server.stop_failing();
    s.witness.check(&s.client, LONG).await.unwrap();
    assert_eq!(
        s.witness.load().unwrap().unanswered,
        0,
        "an answer resets it"
    );
}

/// A log shorter than a checkpoint saved from it was rolled back, which is
/// what restoring a backup does: found like a rewrite.
#[tokio::test]
async fn a_log_rolled_back_is_found() {
    let s = setup(8).await;
    s.client.pull("acme/app").await.unwrap();
    s.server.truncate_audit_log(5);
    let found = s.witness.check(&s.client, LONG).await.unwrap();
    let finding = finding_of(&found);
    assert!(finding.detail.contains("fewer than the 9"), "{finding:?}");
}

/// Two roots for one size need no proof: the pull that sees the second one
/// writes it down there and then.
#[tokio::test]
async fn a_second_root_for_a_size_is_found_by_the_pull() {
    let s = setup(4).await;
    s.client.pull("acme/app").await.unwrap();
    s.server.truncate_audit_log(4);
    s.server.rewrite_audit_leaf(1);
    s.client.pull("acme/app").await.unwrap();
    let saved = s.witness.load().unwrap();
    let finding = saved.inconsistent.expect("found by the pull");
    assert!(
        finding.detail.contains("a second root for size 5"),
        "{finding:?}"
    );
}

/// A rewrite found while the file cannot be written is still the answer,
/// flagged as not saved, and never an error in its place: an error would
/// only warn. Mutation: propagate the write's error.
#[tokio::test]
async fn a_finding_that_cannot_be_saved_is_still_the_answer() {
    let mut s = setup(6).await;
    s.client.pull("acme/app").await.unwrap();
    s.server.truncate_audit_log(4);
    s.witness.wait = Duration::from_millis(100);
    // Another process holding the lock for longer than anyone waits.
    let lock = s.witness.file().with_file_name(LOCK_FILE);
    std::fs::write(&lock, "someone else").unwrap();
    let found = s.witness.check(&s.client, LONG).await.unwrap();
    let Witnessed::Inconsistent { finding, unsaved } = &found else {
        panic!("{found:?}");
    };
    assert!(finding.detail.contains("fewer than"), "{finding:?}");
    assert!(unsaved.is_some(), "flagged as not saved");
    // And an export finding one likewise.
    let tree = Tree::rebuild([merkle::hash_leaf(b"x")]);
    let current = Checkpoint {
        size: 1,
        root: tree.root(),
    };
    let found = s.witness.witness_export(&tree, current).unwrap();
    assert!(
        matches!(
            found,
            Witnessed::Inconsistent {
                unsaved: Some(_),
                ..
            }
        ),
        "{found:?}"
    );
}

/// N10: a check that finds nothing does not mark anything checked beside
/// a finding another process wrote meanwhile, and answers that finding.
/// Mutation: commit without looking for one.
#[test]
fn nothing_is_marked_checked_beside_a_finding_written_meanwhile() {
    let dir = tempfile::tempdir().unwrap();
    let w = Witness::new(dir.path().join(AUDIT_FILE), "https://r.example");
    w.record(&cp(3, 3).header()).unwrap();
    // Meanwhile, another process finds a second root for size 3.
    w.record(&cp(3, 4).header()).unwrap();
    let before = w.load().unwrap();
    assert!(before.inconsistent.is_some());
    w.commit(&[cp(3, 3)], Some(cp(9, 9))).unwrap();
    let after = w.load().unwrap();
    assert_eq!(after, before, "nothing changed beside the finding");
}

/// N11: the first finding stands. A later one, found against another
/// checkpoint or another log, neither replaces it nor is answered in its
/// place. Mutation: write every finding over the last.
#[test]
fn the_first_finding_stands() {
    let dir = tempfile::tempdir().unwrap();
    let w = Witness::new(dir.path().join(AUDIT_FILE), "https://r.example");
    let first = w.found(cp(5, 1), cp(4, 2), "the first".into());
    let second = w.found(cp(7, 1), cp(3, 2), "the second".into());
    assert_eq!(first, second, "the second answers with the first");
    assert_eq!(finding_of(&first).detail, "the first");
    assert_eq!(w.load().unwrap().inconsistent.unwrap().detail, "the first");
}

/// The same root for a size already saved changes nothing.
#[test]
fn the_same_checkpoint_twice_is_one() {
    let dir = tempfile::tempdir().unwrap();
    let w = Witness::new(dir.path().join(AUDIT_FILE), "https://r.example");
    w.record(&cp(5, 1).header()).unwrap();
    w.record(&cp(5, 1).header()).unwrap();
    assert_eq!(w.load().unwrap().unchecked, vec![cp(5, 1)]);
    // Not a checkpoint, or an empty log: ignored.
    w.record("12 not-a-root").unwrap();
    w.record(&cp(0, 0).header()).unwrap();
    assert_eq!(w.load().unwrap().unchecked, vec![cp(5, 1)]);
}

/// When the oldest waiting checkpoint was saved is kept while any waits,
/// through partial progress, and only cleared once none does.
#[test]
fn how_long_they_have_waited_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let w = Witness::new(dir.path().join(AUDIT_FILE), "https://r.example");
    w.record(&cp(3, 3).header()).unwrap();
    let since = w.load().unwrap().unchecked_since.unwrap();
    std::thread::sleep(Duration::from_millis(5));
    w.record(&cp(4, 4).header()).unwrap();
    w.commit(&[cp(3, 3)], None).unwrap();
    assert_eq!(
        w.load().unwrap().unchecked_since,
        Some(since),
        "still waiting"
    );
    w.commit(&[cp(4, 4)], None).unwrap();
    assert_eq!(w.load().unwrap().unchecked_since, None);
}

/// Each server has its own record, and resetting one leaves the others.
#[test]
fn servers_are_kept_apart() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(AUDIT_FILE);
    let a = Witness::new(&file, "https://a.example");
    let b = Witness::new(&file, "https://B.example/");
    a.record(&cp(3, 3).header()).unwrap();
    b.record(&cp(9, 9).header()).unwrap();
    assert_eq!(a.load().unwrap().unchecked, vec![cp(3, 3)]);
    assert_eq!(b.load().unwrap().unchecked, vec![cp(9, 9)]);
    a.reset().unwrap();
    assert!(a.load().unwrap().is_empty());
    assert_eq!(b.load().unwrap().unchecked, vec![cp(9, 9)]);
}

/// A field this build does not know, in the file or in an entry, survives
/// its writes: a newer build's record is not quietly cut down by an older
/// one's pull. Mutation: drop what is not known.
#[test]
fn fields_this_build_does_not_know_are_kept() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(AUDIT_FILE);
    let url = "https://r.example";
    std::fs::write(
        &file,
        serde_json::json!({
            "version": 1,
            "servers": { url: { "checkpoints": [], "unchecked": [], "witnessed_by": "later" } },
            "cosigners": ["later"],
        })
        .to_string(),
    )
    .unwrap();
    let w = Witness::new(&file, url);
    w.record(&cp(5, 5).header()).unwrap();
    let text: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(text["cosigners"], serde_json::json!(["later"]));
    assert_eq!(text["servers"][url]["witnessed_by"], "later");
    assert_eq!(w.load().unwrap().unchecked, vec![cp(5, 5)]);
}

/// A file that cannot be read may hold the only record of a rewrite, so it
/// is never written over; a pull that cannot save its checkpoint still
/// succeeds; and a check says the file is what could not be read.
#[tokio::test]
async fn a_file_that_cannot_be_read_is_never_written_over() {
    let s = setup(2).await;
    std::fs::write(s.witness.file(), b"{ not json").unwrap();
    s.client
        .pull("acme/app")
        .await
        .expect("the pull is not the witness's to fail");
    assert_eq!(std::fs::read(s.witness.file()).unwrap(), b"{ not json");
    assert!(matches!(s.witness.load(), Err(Error::Parse { .. })));
    let err = s.witness.check(&s.client, LONG).await.unwrap_err();
    assert!(err.unreadable() && !err.unanswered(), "{err:?}");

    // One from a newer build is left alone too.
    std::fs::write(s.witness.file(), br#"{"version":2,"servers":{}}"#).unwrap();
    assert!(s.witness.record(&cp(5, 5).header()).is_err());
    assert_eq!(
        std::fs::read(s.witness.file()).unwrap(),
        br#"{"version":2,"servers":{}}"#
    );
}

/// Hooks run at once. Each reads, changes and writes the file under its
/// lock, so none loses another's checkpoint.
#[test]
fn checkpoints_saved_at_once_are_all_kept() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(AUDIT_FILE);
    let threads: Vec<_> = (1..=8u64)
        .map(|size| {
            let w = Witness::new(&file, "https://r.example");
            std::thread::spawn(move || w.record(&cp(size, size as u8).header()).unwrap())
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let w = Witness::new(&file, "https://r.example");
    assert_eq!(w.load().unwrap().unchecked.len(), 8);
}

/// A server older than the audit log answers its routes 404: that is no
/// log, not a rewrite.
#[tokio::test]
async fn a_server_without_a_log_is_not_a_rewrite() {
    let server = FakeServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let witness = Witness::new(dir.path().join(AUDIT_FILE), &server.url);
    let client = Client::new(&server.url, "token")
        .unwrap()
        .with_witness(witness.clone());
    client.pull("acme/app").await.unwrap();
    assert!(
        witness.load().unwrap().is_empty(),
        "no header, nothing saved"
    );
    let err = witness.check(&client, LONG).await.unwrap_err();
    assert!(err.no_log() && !err.unanswered(), "{err:?}");
}

/// Only a server that did not answer at all is unanswered: rate limited,
/// unreachable, or past the deadline. One that answers with anything but a
/// proof (an error status, a body that is not one, a redirect) answered,
/// and did not prove its log: that fails. Mutation: count every client
/// error but a status as unanswered, as before.
#[tokio::test]
async fn only_no_answer_is_unanswered() {
    let s = setup(2).await;
    s.client.pull("acme/app").await.unwrap();
    s.server.fail_with(429, r#"{"error":"too many requests"}"#);
    let err = s
        .witness
        .check(&s.client, Duration::from_millis(200))
        .await
        .unwrap_err();
    assert!(err.unanswered(), "{err:?}");
    for (code, body) in [(500, r#"{"error":"boom"}"#), (200, "not a checkpoint")] {
        s.server.fail_with(code, body);
        let err = s.witness.check(&s.client, LONG).await.unwrap_err();
        assert!(!err.unanswered(), "{code}: {err:?}");
    }
    s.server.redirect_to(302, "http://127.0.0.1:9/elsewhere");
    let err = s.witness.check(&s.client, LONG).await.unwrap_err();
    assert!(!err.unanswered(), "a redirect: {err:?}");
    let dead = Client::new("http://127.0.0.1:9", "token").unwrap();
    let err = s.witness.check(&dead, LONG).await.unwrap_err();
    assert!(err.unanswered(), "unreachable: {err:?}");
    assert_eq!(
        s.witness.load().unwrap().unchecked.len(),
        1,
        "nothing lost meanwhile"
    );
}

/// A proof for other sizes than asked, or one holding something that is not
/// a hash, is the server not answering the question: an error, not a
/// finding about its log.
#[test]
fn a_proof_for_another_question_is_not_a_finding() {
    let held = cp(3, 1);
    let now = cp(9, 2);
    let other = AuditConsistencyResponse {
        first: 4,
        second: 9,
        proof: vec![],
    };
    assert!(matches!(
        prove(held, now, &other),
        Err(CheckError::Malformed(_))
    ));
    let garbage = AuditConsistencyResponse {
        first: 3,
        second: 9,
        proof: vec!["not a hash".into()],
    };
    assert!(matches!(
        prove(held, now, &garbage),
        Err(CheckError::Malformed(_))
    ));
    let wrong = AuditConsistencyResponse {
        first: 3,
        second: 9,
        proof: vec![STANDARD.encode([0u8; 32])],
    };
    assert!(matches!(prove(held, now, &wrong), Ok(Err(_))));
}

/// An export holds every leaf, so it checks each saved checkpoint against
/// their tree with no proof: the same verdicts as a check.
#[tokio::test]
async fn an_export_checks_the_saved_checkpoints_against_its_leaves() {
    let s = setup(4).await;
    s.client.pull("acme/app").await.unwrap();
    s.server.grow_audit_log(3);
    s.client.pull("acme/app").await.unwrap();
    let leaves: Vec<Vec<u8>> = (0..4)
        .map(|i| format!("leaf {i}").into_bytes())
        .chain(["pull 4".into()])
        .chain((5..8).map(|i| format!("leaf {i}").into_bytes()))
        .chain(["pull 8".into()])
        .collect();
    let tree = Tree::rebuild(leaves.iter().map(|l| merkle::hash_leaf(l)));
    let now = Checkpoint::from_wire(&s.server.audit_checkpoint()).unwrap();
    assert_eq!(
        (tree.size(), tree.root()),
        (now.size, now.root),
        "the fake's log, rebuilt"
    );

    let found = s.witness.witness_export(&tree, now).unwrap();
    assert_eq!(
        found,
        Witnessed::Extends {
            current: now,
            proved: 2
        }
    );

    let mut rewritten = leaves.clone();
    rewritten[1] = b"rewritten".to_vec();
    let tree = Tree::rebuild(rewritten.iter().map(|l| merkle::hash_leaf(l)));
    let current = Checkpoint {
        size: tree.size(),
        root: tree.root(),
    };
    let found = s.witness.witness_export(&tree, current).unwrap();
    assert!(
        finding_of(&found).detail.contains("first 5 leaves"),
        "{found:?}"
    );
}

#[test]
fn what_is_known_without_a_proof() {
    assert_eq!(compare(cp(5, 1), cp(5, 1)), Compared::Holds);
    assert!(matches!(compare(cp(5, 1), cp(5, 2)), Compared::Fails(_)));
    assert!(matches!(compare(cp(6, 1), cp(5, 1)), Compared::Fails(_)));
    assert_eq!(compare(cp(4, 1), cp(5, 1)), Compared::NeedsProof);
    let empty = Checkpoint {
        size: 0,
        root: merkle::empty_root(),
    };
    assert_eq!(compare(empty, cp(5, 1)), Compared::Holds);
    assert!(matches!(compare(cp(0, 1), cp(5, 1)), Compared::Fails(_)));
}
