#!/usr/bin/env python3
"""A stand-in for github.com/pimlabs/recall/releases, on loopback, for tests.

Usage: fake-release-server.py <root> <port file>

python3's own http.server, serving <root>, plus the one thing a static
directory cannot do: answer `/releases/latest` with the redirect GitHub
gives, to `/releases/tag/v<version>`, where <version> is read from
<root>/LATEST on every request, so a test can move "latest" without
restarting anything. `/releases/tag/<tag>` answers 200, as GitHub's tag
page does. Everything else is a file under <root>, so a release is laid out
as GitHub's download URLs are:

  <root>/releases/download/v0.4.6/recall-x86_64-unknown-linux-gnu.tar.gz
  <root>/releases/download/v0.4.6/checksums.txt

Point an installer at it with RECALL_TEST_RELEASES_URL=http://127.0.0.1:<port>/releases
(install.sh, install.ps1, npm/install.js), or deploy/fetch-release.sh's
fourth argument at .../releases/download.

Binds 127.0.0.1 only, on a port the OS picks, and writes that port to
<port file> once it is listening. Used by scripts/installer-test.sh and
scripts/installer-test.ps1; nothing that ships runs it.
"""

import functools
import http.server
import sys
from pathlib import Path


class Handler(http.server.SimpleHTTPRequestHandler):
    def send_head(self):
        path = self.path.split("?", 1)[0]
        if path.rstrip("/") == "/releases/latest":
            latest = (Path(self.directory) / "LATEST").read_text().strip()
            self.send_response(302)
            self.send_header("Location", f"/releases/tag/v{latest.lstrip('v')}")
            self.send_header("Content-Length", "0")
            self.end_headers()
            return None
        if path.startswith("/releases/tag/"):
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.send_header("Content-Length", "0")
            self.end_headers()
            return None
        return super().send_head()

    def log_message(self, format, *args):
        sys.stderr.write("fake-release-server: " + (format % args) + "\n")


def main() -> None:
    if len(sys.argv) != 3:
        sys.exit(__doc__.split("\n\n")[1])
    root, port_file = sys.argv[1], Path(sys.argv[2])
    handler = functools.partial(Handler, directory=root)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    port_file.write_text(str(server.server_address[1]))
    server.serve_forever()


if __name__ == "__main__":
    main()
