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
| `push_request`, `push_request_delete` | Written from recall-wire's `PushRequest` at that tag: its field order and its skip rules |

`discovery` exists from the version that introduced `/.well-known/recall`
onwards; older servers answer 404 and have no file.

`0.3.3/` was captured from a development build before 0.3.3 was released,
so its values say `-dev` and its version is not a release. Its shape is the
one 0.3.3 ships. Replace it with a capture from the release archive once
0.3.3 is out; that is the one time a file here is rewritten.

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
