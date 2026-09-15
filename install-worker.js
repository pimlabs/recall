// Cloudflare Worker behind `recall.pimlabs.id`.
//
// Two jobs, and the second one is why this is a Worker rather than a
// Redirect Rule.
//
//   1. Publish the installer at a URL short enough to type:
//
//        curl -fsSL https://recall.pimlabs.id/install | bash
//
//      It is a proxy, not a copy. Every request fetches install.sh from
//      `main`, so there is no second version of that script anywhere and
//      nothing that can fall behind — which matters because install.sh is
//      where the downloaded binary's SHA-256 is checked against the
//      release's checksums.txt. A stale installer is one that verifies
//      nothing, and nobody would notice.
//
//   2. Answer for the hostname's past. `recall.pimlabs.id` was the API's
//      address until 2026-09-15; the API is now `recall-server.pimlabs.id`.
//      A client still pointed here would otherwise get a bare 404 leaked
//      from whatever happens to be at the origin, and Recall's hooks exit 0
//      on an unreachable server by design — so it would stop syncing in
//      total silence. This says what happened, in a place someone debugging
//      will actually look.
//
// Deployed by Cloudflare's Git integration: this repository is connected
// under Workers & Pages, and a push to `main` redeploys. wrangler.toml holds
// the name and the route. Nothing is pasted anywhere — the Worker proxies
// install.sh, so the two must move together, and a dashboard copy would
// drift the moment either changed.
//
// See docs/reference/releasing.md for why each response looks the way it
// does. scripts/install-worker-test.js covers the routing, including the
// case where GitHub is down.

const UPSTREAM =
  "https://raw.githubusercontent.com/pimlabs/recall/main/install.sh";

// `/install` is what the docs publish; `/install.sh` is the spelling people
// type from muscle memory. Both are the same script.
const INSTALL_PATHS = new Set(["/install", "/install.sh"]);

const API_HOST = "recall-server.pimlabs.id";

// The paths Recall's own client and admin page use. A request for one of
// these is not a wrong turn by a human — it is a machine still holding the
// old address, so it gets a 410 and the new one.
const MOVED_PATHS = new Set(["/sync", "/health", "/admin", "/admin/stats"]);

export default {
  async fetch(request) {
    const url = new URL(request.url);

    if (INSTALL_PATHS.has(url.pathname)) {
      const upstream = await fetch(UPSTREAM, {
        cf: { cacheTtl: 60, cacheEverything: true },
      });

      if (!upstream.ok) {
        // Fail loudly rather than serving a truncated script into a shell.
        return new Response(
          `could not fetch the installer from ${UPSTREAM} ` +
            `(upstream said ${upstream.status}). Install another way: ` +
            `npm install -g @pimlabs/recall\n`,
          { status: 502, headers: { "content-type": "text/plain; charset=utf-8" } },
        );
      }

      return new Response(upstream.body, {
        status: 200,
        headers: {
          // GitHub serves this as text/plain. It is a shell script, and
          // nosniff keeps anything downstream from guessing otherwise.
          "content-type": "text/x-sh; charset=utf-8",
          "x-content-type-options": "nosniff",
          "cache-control": "public, max-age=60, must-revalidate",
        },
      });
    }

    if (MOVED_PATHS.has(url.pathname)) {
      // 410 rather than 301: this is not the same resource at a new address.
      // Redirecting a POST /sync would send a client's memory to a host it
      // never authenticated against.
      return new Response(
        `Recall's API moved to https://${API_HOST}${url.pathname}\n` +
          `\n` +
          `This hostname now publishes the installer only.\n` +
          `Update RECALL_URL to https://${API_HOST} and open a new shell.\n`,
        {
          status: 410,
          headers: {
            "content-type": "text/plain; charset=utf-8",
            "x-recall-api": `https://${API_HOST}`,
          },
        },
      );
    }

    return new Response(
      `Not found.\n\n` +
        `  installer : https://recall.pimlabs.id/install\n` +
        `  API       : https://${API_HOST}\n` +
        `  source    : https://github.com/pimlabs/recall\n`,
      { status: 404, headers: { "content-type": "text/plain; charset=utf-8" } },
    );
  },
};
