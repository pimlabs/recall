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

if (!TOKEN) {
  console.error("RECALL_TOKEN is not set. Refusing to start with no auth.");
  process.exit(1);
}

fs.mkdirSync(path.dirname(DB_PATH), { recursive: true });
const db = new DatabaseSync(DB_PATH);
db.exec(`
  CREATE TABLE IF NOT EXISTS memory_files (
    project_key TEXT NOT NULL,
    file_path   TEXT NOT NULL,
    content     TEXT NOT NULL,
    source_env  TEXT,
    updated_at  TEXT NOT NULL,
    PRIMARY KEY (project_key, file_path)
  );
`);

const upsertStmt = db.prepare(`
  INSERT INTO memory_files (project_key, file_path, content, source_env, updated_at)
  VALUES (?, ?, ?, ?, ?)
  ON CONFLICT(project_key, file_path) DO UPDATE SET
    content = excluded.content,
    source_env = excluded.source_env,
    updated_at = excluded.updated_at
`);

const selectStmt = db.prepare(`
  SELECT file_path, content, source_env, updated_at
  FROM memory_files
  WHERE project_key = ?
  ORDER BY file_path
`);

const lastSyncStmt = db.prepare(`SELECT MAX(updated_at) AS last_sync_at FROM memory_files`);

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

  const { project_key, file_path, content, source_env } = parsed;
  if (!project_key || !file_path || typeof content !== "string") {
    return sendJson(res, 400, {
      error: "project_key, file_path, and content (string) are required",
    });
  }
  if (file_path.includes("..") || path.isAbsolute(file_path)) {
    return sendJson(res, 400, { error: "file_path must be relative, no traversal" });
  }

  const updatedAt = new Date().toISOString();
  upsertStmt.run(project_key, file_path, content, source_env || null, updatedAt);

  sendJson(res, 200, { ok: true, project_key, file_path, updated_at: updatedAt });
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
      content: r.content,
      source_env: r.source_env,
      updated_at: r.updated_at,
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

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url, `http://${req.headers.host || "localhost"}`);

  if (url.pathname === "/health" && req.method === "GET") {
    return handleHealth(req, res);
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

  sendJson(res, 404, { error: "not found" });
});

server.listen(PORT, () => {
  console.log(`recall server listening on :${PORT} (db: ${DB_PATH})`);
});
