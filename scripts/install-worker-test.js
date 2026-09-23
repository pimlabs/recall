// Exercises install-worker.js against a stubbed upstream, so the routing is
// tested rather than assumed. Run from the repository root:
//
//   node scripts/install-worker-test.js
import worker from "../install-worker.js";

const SCRIPT = "#!/usr/bin/env bash\necho hello\n";
const PS1_SCRIPT = "Write-Host hello\n";

// Each upstream answers with its own body, so a route that proxied the
// wrong script would fail its body check instead of passing on a shared one.
let upstreamStatus = 200;
let upstreamCalls = 0;
globalThis.fetch = async (url) => {
  upstreamCalls += 1;
  if (upstreamStatus !== 200) return new Response("nope", { status: upstreamStatus });
  const body = String(url).endsWith("/install.ps1") ? PS1_SCRIPT : SCRIPT;
  return new Response(body, { status: 200 });
};

let failures = 0;
function check(label, actual, expected) {
  const ok = actual === expected;
  if (!ok) failures += 1;
  console.log(`  ${ok ? "ok  " : "FAIL"} ${label}${ok ? "" : `: ${actual} != ${expected}`}`);
}

const get = (path, method = "GET") =>
  worker.fetch(new Request(`https://recall.pimlabs.id${path}`, { method }));

console.log("the installer");
for (const path of ["/install", "/install.sh"]) {
  const res = await get(path);
  check(`${path} status`, res.status, 200);
  check(`${path} content-type`, res.headers.get("content-type"), "text/x-sh; charset=utf-8");
  check(`${path} nosniff`, res.headers.get("x-content-type-options"), "nosniff");
  check(`${path} body is the upstream script`, await res.text(), SCRIPT);
}

console.log("\nthe installer (PowerShell)");
{
  const res = await get("/install.ps1");
  check("/install.ps1 status", res.status, 200);
  check("/install.ps1 content-type", res.headers.get("content-type"), "text/plain; charset=utf-8");
  check("/install.ps1 nosniff", res.headers.get("x-content-type-options"), "nosniff");
  check("/install.ps1 body is the upstream install.ps1", await res.text(), PS1_SCRIPT);
}

console.log("\nthe hostname's past");
for (const path of ["/sync", "/health", "/admin", "/admin/stats"]) {
  const res = await get(path, path === "/sync" ? "POST" : "GET");
  check(`${path} is gone, not redirected`, res.status, 410);
  const body = await res.text();
  check(`${path} names the new host`, body.includes("recall-server.pimlabs.id"), true);
  check(`${path} header points at the API`, res.headers.get("x-recall-api"), "https://recall-server.pimlabs.id");
}

console.log("\neverything else");
for (const path of ["/", "/nope", "/install.PS1"]) {
  const res = await get(path);
  check(`${path} is 404`, res.status, 404);
  check(`${path} offers the installer`, (await res.text()).includes("/install"), true);
}

console.log("\na broken upstream");
upstreamStatus = 500;
const res = await get("/install");
check("does not serve a partial script", res.status, 502);
const body = await res.text();
check("says what to do instead", body.includes("npm install -g @pimlabs/recall"), true);
check("never claims to be a shell script", res.headers.get("content-type"), "text/plain; charset=utf-8");

console.log(`\nupstream fetched ${upstreamCalls} time(s)`);
console.log(failures === 0 ? "all checks passed" : `${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
