// Exercises install-worker.js against a stubbed upstream, so the routing is
// tested rather than assumed. Run: node install-worker.test.js
import worker from "./install-worker.js";

const SCRIPT = "#!/usr/bin/env bash\necho hello\n";

let upstreamStatus = 200;
let upstreamCalls = 0;
globalThis.fetch = async () => {
  upstreamCalls += 1;
  return upstreamStatus === 200
    ? new Response(SCRIPT, { status: 200 })
    : new Response("nope", { status: upstreamStatus });
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

console.log("\nthe hostname's past");
for (const path of ["/sync", "/health", "/admin", "/admin/stats"]) {
  const res = await get(path, path === "/sync" ? "POST" : "GET");
  check(`${path} is gone, not redirected`, res.status, 410);
  const body = await res.text();
  check(`${path} names the new host`, body.includes("recall-server.pimlabs.id"), true);
  check(`${path} header points at the API`, res.headers.get("x-recall-api"), "https://recall-server.pimlabs.id");
}

console.log("\neverything else");
for (const path of ["/", "/nope", "/install.ps1"]) {
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
