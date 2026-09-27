# Wire fixtures

What each released version of Recall put on the wire, byte for byte, one
directory per version. `tests/golden.rs` reads every file here with today's
types, and fails if an old shape no longer parses or the newest shape of a
kind has lost a field. `recall-server`'s tests also POST every request
fixture to today's server and expect a 200.

This is the check behind the compatibility promise in
`docs/reference/api.md`: a newer server keeps serving older clients, and a
newer client keeps reading older servers.

## Where each file came from

| Kind | Source |
| --- | --- |
| `push_response`, `push_response_delete`, `sync_response`, `health`, `admin_stats`, `error`, `discovery` | Captured from that version's release archive by `scripts/capture-wire-fixtures.sh` |
| `enroll_response_pending`, `enroll_response_approved`, `enroll_poll_response`, `enroll_poll_error`, `device_pending_response`, `device_approve_response`, `device_me_response`, `device_deny_response`, `device_list_response`, `device_revoke_response`, `authkey_create_response`, `authkey_list_response`, `authkey_revoke_response` | The same script: one enrolment followed through, each response a real one from the step before |
| `push_request`, `push_request_delete` | Written from recall-wire's `PushRequest` at that tag: its field order and its skip rules |
| `enroll_request`, `enroll_request_with_authkey`, `enroll_poll_request`, `device_approve_request`, `device_deny_request`, `authkey_create_request`, `authkey_revoke_request` | Written the same way, from recall-wire's `devices` types |
| `audit_checkpoint_response`, `audit_entries_response`, `audit_consistency_response` | The same script, after the enrolment above and a push that device signs: the checkpoint, every leaf, and the proof from size 1, with nothing appended between them |
| `audit_leaf_push`, `audit_leaf_approve`, `audit_leaf_enroll` | Three of those leaves, byte for byte as the entries page holds them: the signed push, the approve that carries its key, and the enrolment the authkey made. `tests/golden.rs` checks them as the offline verifier would: the push's signature against the approve's key, its digest and keyid, and the page's leaves against the checkpoint's root |
| `push_response_queued`, `job_claim_response`, `job_claim_response_empty`, `job_result_response`, `job_list_response` | The same script: one conflict queued for a worker enrolled on a second scratch server, claimed and merged, each response a real one |
| `job_claim_request`, `job_result_request`, `job_result_request_error`, `device_approve_request_worker` | Written from recall-wire's `jobs` and `devices` types |
| `evaluation_created_response`, `job_claim_response_evaluate`, `evaluation_list_response`, `evaluation_response` | The same script, on the queue's server and worker: one evaluation asked for, claimed with the file the conflict wrote, and reported, each response a real one |
| `evaluation_request`, `job_result_request_evaluate` | Written from recall-wire's `evaluations` and `jobs` types; the report is the one the script posts |

`discovery` exists from the version that introduced `/.well-known/recall`
onwards; older servers answer 404 and have no file. The device kinds exist
from 0.4.1. Their public key is RFC 9421's `test-key-ed25519`, whose
private half is published, and their ids and authkey belong to the
scratch database the capture ran against, which no longer exists.
`device_me_response` answers a signed request, which the script makes with
`openssl` and that key's published private half, so it also shows a
signature Recall's own code did not make being accepted.

`0.4.0/` was captured from a development build, so its values say `-dev`.
Its shape is the one 0.4.0 was built with (there was no 0.3.3; the
discovery document went out with 0.4.0). `v0.4.0` was tagged but its
release run failed before publishing, so there is no 0.4.0 archive to
capture again from, and this directory stays as it is.

`0.4.1/` holds only what 0.4.1 adds or changes (the device kinds, and
`discovery`, which gained `device-sig-v1` and the `devices` capability). It
was first captured from a development build and captured again from the
v0.4.1 release archive once 0.4.1 was out, so `discovery` also carries the
`created` stamp a release build reports.

`0.4.2/` holds what the merge queue and the audit log add or change: the
job kinds, `push_response_queued` (a push answered with the `merge_job` it
queued), `health` (with the `worker` and `queue` members a server with a
worker reports), `device_approve_request_worker`, the audit kinds, and
`discovery` (with `merge_queue` and `audit`). It was first captured from a
development build and captured again from the v0.4.2 release archive once
0.4.2 was out. Its worker, and the push in the audit kinds, sign with the
same published test key the device kinds use, with `openssl`.

`0.4.5/` holds what evaluation reports add or change: the evaluation
kinds, the `evaluate` claim and result, and `discovery` (with
`evaluation`). It was first captured from a development build and
captured again from the v0.4.5 release archive once 0.4.5 was out, so
`discovery` also carries the `created` stamp a release build reports.

## Rules

- **Never edit a fixture of a released version.** It records what that
  release sent. If today's types cannot read it, the types are wrong, not
  the fixture.
- **When a shape changes, add a directory** for the release that changes it,
  captured from the release archive:

  ```sh
  ./scripts/capture-wire-fixtures.sh 0.3.4
  ```

  The script keeps any file that already exists. Write the request fixtures
  by hand if `PushRequest` changed. It downloads the archive under the name
  that version was released with: `recall-server_linux_<arch>.tar.gz` up to
  0.4.5, `recall-server-<target>.tar.gz` (Linux only) from 0.4.6, and the
  client's own archive before 0.4.0.
- **A new kind of request or response gets a fixture** and an arm in
  `round_trip` in `tests/golden.rs`. A file whose kind that function does
  not know fails the test, so a fixture cannot be added and never read.
