#!/usr/bin/env node
// Recall sync server — Phase 0: SQLite storage, last-write-wins, no merge logic.
// No external dependencies: node:http + node:sqlite (Node >= 22.5).

const http = require("node:http");
const { DatabaseSync } = require("node:sqlite");
const path = require("node:path");
const fs = require("node:fs");
const crypto = require("node:crypto");

const PORT = process.env.RECALL_PORT || 8787;
const TOKEN = process.env.RECALL_TOKEN;
const DB_PATH = process.env.RECALL_DB_PATH || path.join(__dirname, "data", "recall.db");
const GIT_COMMIT = process.env.RECALL_GIT_COMMIT || "unknown";
const BACKUP_DIR = process.env.RECALL_BACKUP_DIR || "";
const BACKUP_INTERVAL_HOURS = Number(process.env.RECALL_BACKUP_INTERVAL_HOURS || 24);
const BACKUP_KEEP = Number(process.env.RECALL_BACKUP_KEEP || 7);
const RATE_LIMIT_WINDOW_MS = Number(process.env.RECALL_RATE_LIMIT_WINDOW_MS || 60_000);
const RATE_LIMIT_MAX = Number(process.env.RECALL_RATE_LIMIT_MAX || 60);

if (!TOKEN) {
  console.error("RECALL_TOKEN is not set. Refusing to start with no auth.");
  process.exit(1);
}

// Static markup only — no data is embedded server-side. The page fetches
// /admin/stats client-side with a bearer token the user supplies, so this
// route itself carries nothing worth gating.
const ADMIN_PAGE_HTML = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Recall admin</title>
<style>
  :root {
    color-scheme: light dark;
    --bg: #ffffff;
    --fg: #1a1a1a;
    --muted: #6b7280;
    --border: #e5e7eb;
    --row-alt: #fafafa;
    --error: #b91c1c;
    --accent: #111827;
  }
  @media (prefers-color-scheme: dark) {
    :root {
      --bg: #14161a;
      --fg: #e8e8e8;
      --muted: #9aa0a6;
      --border: #2a2d33;
      --row-alt: #1b1d22;
      --error: #f87171;
      --accent: #e8e8e8;
    }
  }
  * { box-sizing: border-box; }
  body {
    margin: 0;
    padding: 2.5rem 1.5rem;
    background: var(--bg);
    color: var(--fg);
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
  }
  main {
    max-width: 860px;
    margin: 0 auto;
  }
  h1 {
    font-size: 1.25rem;
    font-weight: 600;
    margin: 0 0 1.5rem;
  }
  .token-row {
    display: flex;
    gap: 0.5rem;
    margin-bottom: 1.75rem;
  }
  input[type="password"], input[type="text"] {
    flex: 1;
    padding: 0.5rem 0.75rem;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: var(--bg);
    color: var(--fg);
    font-size: 0.9rem;
  }
  button {
    padding: 0.5rem 1rem;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: var(--accent);
    color: var(--bg);
    font-size: 0.9rem;
    cursor: pointer;
  }
  button:hover { opacity: 0.85; }
  .summary {
    color: var(--muted);
    font-size: 0.9rem;
    margin-bottom: 1.5rem;
    line-height: 1.6;
  }
  .error {
    color: var(--error);
    font-size: 0.9rem;
    margin-bottom: 1.5rem;
  }
  table {
    width: 100%;
    border-collapse: collapse;
    font-size: 0.9rem;
  }
  th, td {
    text-align: left;
    padding: 0.6rem 0.75rem;
    border-bottom: 1px solid var(--border);
  }
  th {
    color: var(--muted);
    font-weight: 500;
    font-size: 0.8rem;
    text-transform: uppercase;
    letter-spacing: 0.03em;
  }
  tbody tr:nth-child(even) { background: var(--row-alt); }
  .empty {
    color: var(--muted);
    font-size: 0.9rem;
  }
</style>
</head>
<body>
<main>
  <h1>Recall admin</h1>
  <div class="token-row">
    <input type="password" id="token" placeholder="Bearer token">
    <button id="load">Load</button>
  </div>
  <div id="summary" class="summary"></div>
  <div id="error" class="error" style="display:none"></div>
  <table id="table" style="display:none">
    <thead>
      <tr>
        <th>Project</th>
        <th>Active files</th>
        <th>Deleted</th>
        <th>Sources</th>
        <th>Last updated</th>
      </tr>
    </thead>
    <tbody id="tbody"></tbody>
  </table>
</main>
<script>
(function () {
  var STORAGE_KEY = "recall_admin_token";
  var tokenInput = document.getElementById("token");
  var loadBtn = document.getElementById("load");
  var summaryEl = document.getElementById("summary");
  var errorEl = document.getElementById("error");
  var tableEl = document.getElementById("table");
  var tbodyEl = document.getElementById("tbody");

  function clearChildren(el) {
    while (el.firstChild) el.removeChild(el.firstChild);
  }

  function showError(message) {
    tableEl.style.display = "none";
    clearChildren(summaryEl);
    errorEl.textContent = message;
    errorEl.style.display = "block";
  }

  function hideError() {
    errorEl.style.display = "none";
    errorEl.textContent = "";
  }

  function renderSummary(data) {
    clearChildren(summaryEl);
    var totals = data.totals || {};
    var parts = [
      "Projects: " + totals.project_count,
      "Active files: " + totals.file_count,
      "Deleted: " + totals.deleted_count,
      "git_commit: " + data.git_commit,
      "last_backup_at: " + (data.last_backup_at || "never"),
    ];
    parts.forEach(function (text, i) {
      if (i > 0) summaryEl.appendChild(document.createTextNode(" · "));
      summaryEl.appendChild(document.createTextNode(text));
    });
  }

  function renderTable(projects) {
    clearChildren(tbodyEl);
    if (!projects.length) {
      var tr = document.createElement("tr");
      var td = document.createElement("td");
      td.colSpan = 5;
      td.className = "empty";
      td.textContent = "No projects yet.";
      tr.appendChild(td);
      tbodyEl.appendChild(tr);
      tableEl.style.display = "";
      return;
    }
    projects.forEach(function (p) {
      var tr = document.createElement("tr");

      var tdKey = document.createElement("td");
      tdKey.textContent = p.project_key;
      tr.appendChild(tdKey);

      var tdActive = document.createElement("td");
      tdActive.textContent = String(p.file_count);
      tr.appendChild(tdActive);

      var tdDeleted = document.createElement("td");
      tdDeleted.textContent = String(p.deleted_count);
      tr.appendChild(tdDeleted);

      var tdSources = document.createElement("td");
      tdSources.textContent = (p.sources || []).join(", ");
      tr.appendChild(tdSources);

      var tdUpdated = document.createElement("td");
      tdUpdated.textContent = p.last_updated_at || "";
      tr.appendChild(tdUpdated);

      tbodyEl.appendChild(tr);
    });
    tableEl.style.display = "";
  }

  function load() {
    var token = tokenInput.value.trim();
    if (!token) {
      showError("enter a token");
      return;
    }
    hideError();
    fetch("/admin/stats", { headers: { Authorization: "Bearer " + token } })
      .then(function (resp) {
        if (resp.status === 401) {
          showError("invalid token");
          return null;
        }
        if (!resp.ok) {
          showError("request failed (" + resp.status + ")");
          return null;
        }
        // Only persist a token that's actually been proven valid.
        sessionStorage.setItem(STORAGE_KEY, token);
        return resp.json();
      })
      .then(function (data) {
        if (!data) return;
        renderSummary(data);
        renderTable(data.projects || []);
      })
      .catch(function () {
        showError("request failed");
      });
  }

  loadBtn.addEventListener("click", load);
  tokenInput.addEventListener("keydown", function (e) {
    if (e.key === "Enter") load();
  });

  var stored = sessionStorage.getItem(STORAGE_KEY);
  if (stored) {
    tokenInput.value = stored;
    load();
  }
})();
</script>
</body>
</html>
`;

fs.mkdirSync(path.dirname(DB_PATH), { recursive: true });
const db = new DatabaseSync(DB_PATH);
db.exec(`
  CREATE TABLE IF NOT EXISTS memory_files (
    project_key TEXT NOT NULL,
    file_path   TEXT NOT NULL,
    content     TEXT NOT NULL,
    source_env  TEXT,
    updated_at  TEXT NOT NULL,
    deleted     INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (project_key, file_path)
  );
`);

// Migration for databases created before the `deleted` column existed —
// ALTER TABLE ADD COLUMN is safe to run against an already-migrated
// database too, guarded so it only runs once.
const hasDeletedColumn = db
  .prepare("PRAGMA table_info(memory_files)")
  .all()
  .some((c) => c.name === "deleted");
if (!hasDeletedColumn) {
  db.exec("ALTER TABLE memory_files ADD COLUMN deleted INTEGER NOT NULL DEFAULT 0");
}

// Tombstone, not delete: content is preserved (ON CONFLICT leaves it
// untouched) so a mistaken delete is recoverable at the database level,
// even though nothing in the app surfaces an "undo" yet.
const upsertStmt = db.prepare(`
  INSERT INTO memory_files (project_key, file_path, content, source_env, updated_at, deleted)
  VALUES (?, ?, ?, ?, ?, 0)
  ON CONFLICT(project_key, file_path) DO UPDATE SET
    content = excluded.content,
    source_env = excluded.source_env,
    updated_at = excluded.updated_at,
    deleted = 0
`);

const tombstoneStmt = db.prepare(`
  INSERT INTO memory_files (project_key, file_path, content, source_env, updated_at, deleted)
  VALUES (?, ?, '', ?, ?, 1)
  ON CONFLICT(project_key, file_path) DO UPDATE SET
    source_env = excluded.source_env,
    updated_at = excluded.updated_at,
    deleted = 1
`);

const selectStmt = db.prepare(`
  SELECT file_path, content, source_env, updated_at, deleted
  FROM memory_files
  WHERE project_key = ?
  ORDER BY file_path
`);

const lastSyncStmt = db.prepare(`SELECT MAX(updated_at) AS last_sync_at FROM memory_files`);

const adminProjectsStmt = db.prepare(`
  SELECT
    project_key,
    SUM(CASE WHEN deleted = 0 THEN 1 ELSE 0 END) AS file_count,
    SUM(CASE WHEN deleted = 1 THEN 1 ELSE 0 END) AS deleted_count,
    MAX(updated_at) AS last_updated_at
  FROM memory_files
  GROUP BY project_key
  ORDER BY last_updated_at DESC
`);

// Kept separate from adminProjectsStmt rather than folded in via
// GROUP_CONCAT(DISTINCT ...): SQLite doesn't allow a custom separator
// together with DISTINCT, so the separator is forced to ',' — and
// source_env is client-supplied and unvalidated, so a value containing a
// comma would silently split into bogus extra entries.
const adminSourcesStmt = db.prepare(`
  SELECT DISTINCT project_key, source_env
  FROM memory_files
  WHERE source_env IS NOT NULL
`);

const startedAt = new Date().toISOString();
let lastBackupAt = null;

function runBackup() {
  if (!BACKUP_DIR) return;
  fs.mkdirSync(BACKUP_DIR, { recursive: true });
  const stamp = new Date().toISOString().replace(/[:.]/g, "-");
  const dest = path.join(BACKUP_DIR, `recall-${stamp}.db`);
  // VACUUM INTO takes a consistent snapshot even while the server keeps
  // writing — safe to run on a live database, unlike copying the file.
  db.prepare("VACUUM INTO ?").run(dest);
  lastBackupAt = new Date().toISOString();
  console.log(`backup written: ${dest}`);

  const files = fs
    .readdirSync(BACKUP_DIR)
    .filter((f) => f.startsWith("recall-") && f.endsWith(".db"))
    .sort();
  const excess = files.length - BACKUP_KEEP;
  for (let i = 0; i < excess; i++) {
    fs.unlinkSync(path.join(BACKUP_DIR, files[i]));
    console.log(`backup pruned: ${files[i]}`);
  }
}

function runBackupSafely() {
  try {
    runBackup();
  } catch (err) {
    // A backup failure must never take the sync server down with it.
    console.error(`backup failed: ${err.message}`);
  }
}

if (BACKUP_DIR) {
  runBackupSafely();
  // setInterval's delay is a 32-bit signed int (~24.8 days max) — past
  // that it silently becomes ~1ms instead of erroring, which turns one
  // misconfigured env var into a backup-storm. Clamp well under the limit.
  const MAX_INTERVAL_HOURS = 24 * 20; // 20 days
  const intervalHours = Math.min(BACKUP_INTERVAL_HOURS, MAX_INTERVAL_HOURS);
  setInterval(runBackupSafely, intervalHours * 60 * 60 * 1000).unref();
}

// In-memory, per-IP fixed-window limiter — no external store needed for a
// single-process personal server. Applies to /sync only (both valid and
// invalid-token requests count, so a flood of bad tokens can't dodge it by
// never reaching isAuthorized); /health stays unlimited since it's meant
// to be pollable by uptime tools and is cheap regardless.
const rateLimitBuckets = new Map();

function getClientIp(req) {
  // Every request that reaches this process already came through the
  // Cloudflare Tunnel — the origin port isn't published anywhere else
  // (deploy/docker-compose.yml uses `expose`, not `ports`) — so this
  // header can't be spoofed by hitting the origin directly.
  const cfIp = req.headers["cf-connecting-ip"];
  if (cfIp) return cfIp;
  const xff = req.headers["x-forwarded-for"];
  if (xff) return xff.split(",")[0].trim();
  return req.socket.remoteAddress || "unknown";
}

function isRateLimited(ip) {
  const now = Date.now();
  let bucket = rateLimitBuckets.get(ip);
  if (!bucket || now - bucket.windowStart >= RATE_LIMIT_WINDOW_MS) {
    bucket = { count: 0, windowStart: now };
    rateLimitBuckets.set(ip, bucket);
  }
  bucket.count += 1;
  return bucket.count > RATE_LIMIT_MAX;
}

// Otherwise every distinct IP that ever hits the server stays in memory
// forever. Sweep buckets that are well past their window.
setInterval(() => {
  const now = Date.now();
  for (const [ip, bucket] of rateLimitBuckets) {
    if (now - bucket.windowStart >= RATE_LIMIT_WINDOW_MS * 2) {
      rateLimitBuckets.delete(ip);
    }
  }
}, RATE_LIMIT_WINDOW_MS).unref();

function isAuthorized(req) {
  const header = req.headers["authorization"] || "";
  const [scheme, value] = header.split(" ");
  if (scheme !== "Bearer" || !value) return false;
  const a = Buffer.from(value);
  const b = Buffer.from(TOKEN);
  return a.length === b.length && crypto.timingSafeEqual(a, b);
}

function readBody(req) {
  return new Promise((resolve, reject) => {
    let chunks = [];
    let size = 0;
    req.on("data", (chunk) => {
      size += chunk.length;
      if (size > 5 * 1024 * 1024) {
        reject(new Error("body too large"));
        req.destroy();
        return;
      }
      chunks.push(chunk);
    });
    req.on("end", () => resolve(Buffer.concat(chunks).toString("utf8")));
    req.on("error", reject);
  });
}

function sendJson(res, status, body) {
  const data = JSON.stringify(body);
  res.writeHead(status, { "Content-Type": "application/json" });
  res.end(data);
}

async function handleSyncPost(req, res) {
  let parsed;
  try {
    parsed = JSON.parse(await readBody(req));
  } catch {
    return sendJson(res, 400, { error: "invalid json body" });
  }

  const { project_key, file_path, content, source_env, deleted } = parsed;
  const isDelete = deleted === true;
  if (!project_key || !file_path || (!isDelete && typeof content !== "string")) {
    return sendJson(res, 400, {
      error: "project_key, file_path, and content (string) are required, unless deleted is true",
    });
  }
  if (file_path.includes("..") || path.isAbsolute(file_path)) {
    return sendJson(res, 400, { error: "file_path must be relative, no traversal" });
  }

  const updatedAt = new Date().toISOString();
  if (isDelete) {
    tombstoneStmt.run(project_key, file_path, source_env || null, updatedAt);
  } else {
    upsertStmt.run(project_key, file_path, content, source_env || null, updatedAt);
  }

  sendJson(res, 200, { ok: true, project_key, file_path, deleted: isDelete, updated_at: updatedAt });
}

function handleSyncGet(req, res, url) {
  const projectKey = url.searchParams.get("project_key");
  if (!projectKey) {
    return sendJson(res, 400, { error: "project_key query param is required" });
  }
  const rows = selectStmt.all(projectKey);
  sendJson(res, 200, {
    project_key: projectKey,
    files: rows.map((r) => ({
      file_path: r.file_path,
      // Tombstoned rows keep their content in the database (for a
      // possible future undo) but don't hand it back over the wire —
      // a pull shouldn't be able to resurrect a deleted file's content.
      content: r.deleted ? null : r.content,
      source_env: r.source_env,
      updated_at: r.updated_at,
      deleted: !!r.deleted,
    })),
  });
}

function handleHealth(req, res) {
  const { last_sync_at } = lastSyncStmt.get();
  sendJson(res, 200, {
    status: "ok",
    git_commit: GIT_COMMIT,
    started_at: startedAt,
    last_sync_at: last_sync_at || null,
    last_backup_at: lastBackupAt,
  });
}

function handleAdminStats(req, res) {
  const rows = adminProjectsStmt.all();
  const sourcesByProject = new Map();
  for (const { project_key, source_env } of adminSourcesStmt.all()) {
    if (!sourcesByProject.has(project_key)) sourcesByProject.set(project_key, []);
    sourcesByProject.get(project_key).push(source_env);
  }
  const projects = rows.map((r) => ({
    project_key: r.project_key,
    file_count: r.file_count,
    deleted_count: r.deleted_count,
    sources: sourcesByProject.get(r.project_key) || [],
    last_updated_at: r.last_updated_at,
  }));
  const totals = projects.reduce(
    (acc, p) => {
      acc.file_count += p.file_count;
      acc.deleted_count += p.deleted_count;
      return acc;
    },
    { project_count: projects.length, file_count: 0, deleted_count: 0 }
  );
  sendJson(res, 200, {
    projects,
    totals,
    git_commit: GIT_COMMIT,
    last_backup_at: lastBackupAt,
  });
}

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url, `http://${req.headers.host || "localhost"}`);

  if (url.pathname === "/health" && req.method === "GET") {
    return handleHealth(req, res);
  }

  if (url.pathname === "/admin" && req.method === "GET") {
    res.writeHead(200, {
      "Content-Type": "text/html; charset=utf-8",
      "X-Content-Type-Options": "nosniff",
      // The bearer token lives in sessionStorage on this origin for the
      // life of the tab — default-src 'none' plus connect-src 'self' means
      // even a future script-injection bug here has nowhere to exfiltrate
      // it to.
      "Content-Security-Policy":
        "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'",
    });
    return res.end(ADMIN_PAGE_HTML);
  }

  if (isRateLimited(getClientIp(req))) {
    res.setHeader("Retry-After", Math.ceil(RATE_LIMIT_WINDOW_MS / 1000));
    return sendJson(res, 429, { error: "rate limit exceeded, try again later" });
  }

  if (!isAuthorized(req)) {
    return sendJson(res, 401, { error: "unauthorized" });
  }

  if (url.pathname === "/sync" && req.method === "POST") {
    return handleSyncPost(req, res);
  }
  if (url.pathname === "/sync" && req.method === "GET") {
    return handleSyncGet(req, res, url);
  }
  if (url.pathname === "/admin/stats" && req.method === "GET") {
    return handleAdminStats(req, res);
  }

  sendJson(res, 404, { error: "not found" });
});

server.listen(PORT, () => {
  console.log(`recall server listening on :${PORT} (db: ${DB_PATH})`);
});
