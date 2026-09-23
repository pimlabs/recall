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
| `audit_checkpoint_response`, `audit_entries_response`, `audit_consistency_response` | Written from recall-wire's `audit` types |
| `audit_leaf_push`, `audit_leaf_approve` | A leaf exactly as `recall_server::audit::leaf::encode` writes one, for the `push` and `approve` actions — hand-built rather than captured, since a leaf's shape lives in `recall-server`, not this crate; see `docs/design/part5-plan.md`'s "PR 1: audit" for the field-by-field meaning |

`discovery` exists from the version that introduced `/.well-known/recall`
onwards; older servers answer 404 and have no file. The device kinds exist
from 0.4.1. Their public key is RFC 9421's `test-key-ed25519`, whose
private half is published, and their ids and authkey belong to the
scratch database the capture ran against, which no longer exists.
`device_me_response` answers a signed request, which the script makes with
`openssl` and that key's published private half, so it also shows a
signature Recall's own code did not make being accepted.

`0.4.0/` was captured from a development build before 0.4.0 was released,
so its values say `-dev` and its version is not a release. Its shape is the
one 0.4.0 ships (there was no 0.3.3; the discovery document went out with
0.4.0). Replace it with a capture from the release archive once 0.4.0 is
out; that is the one time a file here is rewritten.

`0.4.1/` is the same: captured from a development build with
`./scripts/capture-wire-fixtures.sh 0.4.1 target/release/recall-server`,
holding only what 0.4.1 adds or changes (the device kinds, and `discovery`,
which gained `device-sig-v1` and the `devices` capability). Being
unreleased, it was captured again as the device shapes changed during
review, and again for the audit routes and `discovery`'s `audit`
capability (part of the same unreleased version); replace it once more
from the release archive when 0.4.1 is out.

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
  by hand if `PushRequest` changed.
- **A new kind of request or response gets a fixture** and an arm in
  `round_trip` in `tests/golden.rs`. A file whose kind that function does
  not know fails the test, so a fixture cannot be added and never read.
