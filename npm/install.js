#!/usr/bin/env node
// Fetches the prebuilt `recall` binary for this platform after npm install.
//
// A downloader rather than per-platform optional dependencies: this is a
// single-owner tool, and publishing five packages per release to save one
// download is not a trade worth making here. The download is verified
// against the release's checksums.txt before anything is made executable.
"use strict";

const fs = require("node:fs");
const path = require("node:path");
const crypto = require("node:crypto");
const zlib = require("node:zlib");
const { execFileSync } = require("node:child_process");

const REPO = "pimlabs/recall";
const version = require("./package.json").version;
const binDir = path.join(__dirname, "bin");
const binPath = path.join(binDir, process.platform === "win32" ? "recall-bin.exe" : "recall-bin");

// RECALL_TEST_RELEASES_URL is for this repository's CI and nothing else: it
// replaces https://github.com/pimlabs/recall/releases, so
// scripts/installer-test.sh can serve a release from loopback. The checksums
// come from the same place as the archive, so pointing it anywhere else
// would verify nothing; anything but plain http to a loopback address and a
// port is refused rather than used.
const LOOPBACK = /^http:\/\/(127\.0\.0\.1|localhost|\[::1\]):\d+(\/.*)?$/;
function releasesUrl() {
  const test = process.env.RECALL_TEST_RELEASES_URL;
  if (!test) {
    return `https://github.com/${REPO}/releases`;
  }
  if (!LOOPBACK.test(test)) {
    fail(
      `RECALL_TEST_RELEASES_URL is only for this repository's tests, and only ` +
        `http://127.0.0.1:<port>, http://localhost:<port> or http://[::1]:<port>; ` +
        `got ${test}. Unset it.`
    );
  }
  return test.replace(/\/+$/, "");
}

// v0.4.5 is the last release whose archives are named recall_<os>_<arch>
// and hold a bare binary (renamed like the archive in the tar.gz, plain
// recall.exe in the zip). Every release after it names them
// recall-<rust target>, holding a recall-<rust target>/ directory with the
// binary in it. Each npm version fetches its own version's archive, so this
// only matters for a package at 0.4.5 or older, and tags never move.
const LAST_OLD_STYLE_RELEASE = "0.4.5";

// A pre-release counts as the version it leads up to.
function usesOldArchiveNames(v) {
  const parts = (s) => s.split(/[-+]/)[0].split(".").map(Number);
  const [a, b] = [parts(v), parts(LAST_OLD_STYLE_RELEASE)];
  for (let i = 0; i < 3; i++) {
    if (a[i] !== b[i]) return a[i] < b[i];
  }
  return true;
}

// `target` is the Rust target the release builds for this platform, which
// names the archive; `old` is what v0.4.5 and older called it.
const PLATFORMS = {
  "darwin:x64": { target: "x86_64-apple-darwin", old: "darwin_amd64", ext: "tar.gz" },
  "darwin:arm64": { target: "aarch64-apple-darwin", old: "darwin_arm64", ext: "tar.gz" },
  "linux:x64": { target: "x86_64-unknown-linux-gnu", old: "linux_amd64", ext: "tar.gz" },
  "linux:arm64": { target: "aarch64-unknown-linux-gnu", old: "linux_arm64", ext: "tar.gz" },
  "win32:x64": { target: "x86_64-pc-windows-msvc", old: "windows_amd64", ext: "zip" },
  "win32:arm64": { target: "aarch64-pc-windows-msvc", old: "windows_arm64", ext: "zip" },
};

// The archive to download, and the binary's path inside it once extracted.
function archiveFor({ target, old, ext }, v) {
  const exe = ext === "zip" ? "recall.exe" : "recall";
  if (usesOldArchiveNames(v)) {
    return { asset: `recall_${old}.${ext}`, inner: ext === "zip" ? exe : `recall_${old}` };
  }
  return { asset: `recall-${target}.${ext}`, inner: path.join(`recall-${target}`, exe) };
}

function fail(message) {
  console.error(`recall: ${message}`);
  process.exit(1);
}

async function download(url) {
  const res = await fetch(url, { redirect: "follow" });
  if (!res.ok) {
    throw new Error(`${res.status} ${res.statusText} for ${url}`);
  }
  return Buffer.from(await res.arrayBuffer());
}

// tar.gz or zip. Rather than pull in a library for either, shell out to the
// `tar` every supported platform already has — including Windows, where the
// built-in tar.exe (bsdtar, via libarchive) has shipped since Windows 10 and
// reads zip just as well as gzip, auto-detecting the format from the file's
// own bytes rather than its name. Unpacked into a directory of its own
// beside bin/, so the rename out of it never crosses a filesystem, and
// nothing but the binary is left behind.
function extractBinary(archive, inner, ext) {
  const isZip = ext === "zip";
  const staging = fs.mkdtempSync(path.join(__dirname, ".recall-extract-"));
  try {
    const archivePath = path.join(staging, isZip ? "recall.zip" : "recall.tar");
    fs.writeFileSync(archivePath, isZip ? archive : zlib.gunzipSync(archive));
    execFileSync("tar", ["-xf", archivePath, "-C", staging]);
    const extracted = path.join(staging, inner);
    // lstat, not exists: the entry itself must be a regular file. A symlink
    // in the archive would otherwise be renamed into bin/ and then
    // chmod-ed, which follows it and changes whatever it points at.
    let entry;
    try {
      entry = fs.lstatSync(extracted);
    } catch {
      throw new Error(`archive did not contain ${inner}`);
    }
    if (!entry.isFile()) {
      throw new Error(`${inner} in the archive is not a regular file`);
    }
    fs.renameSync(extracted, binPath);
  } finally {
    fs.rmSync(staging, { recursive: true, force: true });
  }
}

// On macOS and Linux, npm's bin link is a symlink to bin/recall, which ships
// as a Node shim. The shim works, but it starts Node on every call, and
// `recall push` runs on every memory write in a session: measured, 52 ms a
// call through the shim against 6 ms for the binary itself. So once the
// binary is verified it replaces the shim at that path, and the symlink
// then points straight at it — the optimization esbuild's installer makes
// for the same reason. The shim stays on Windows, where npm wraps bin files
// in .cmd and .ps1 scripts that need something Node can run.
//
// A rename, so the path always holds either the shim or the whole binary.
// If it fails the shim is still there, still works, and is only slower.
function putBinaryWhereTheLinkPoints() {
  const shim = path.join(binDir, "recall");
  try {
    fs.renameSync(binPath, shim);
    return shim;
  } catch (err) {
    console.warn(`recall: kept the Node shim (${err.message}); every call starts Node first`);
    return binPath;
  }
}

async function main() {
  const key = `${process.platform}:${process.arch}`;
  const platform = PLATFORMS[key];
  if (!platform) {
    fail(
      `unsupported platform ${key}. macOS, Linux and Windows on x64/arm64 only. ` +
        `See https://github.com/${REPO}`
    );
  }
  const { asset, inner } = archiveFor(platform, version);
  const base = `${releasesUrl()}/download/v${version}`;

  try {
    const [archive, checksums] = await Promise.all([
      download(`${base}/${asset}`),
      download(`${base}/checksums.txt`),
    ]);

    // Never make something usable that hasn't been checked against the
    // release's own manifest.
    const actual = crypto.createHash("sha256").update(archive).digest("hex");
    // The line naming exactly this archive: sha256sum writes `<hash>  <name>`,
    // or `<hash> *<name>` in binary mode.
    const expected = checksums
      .toString("utf8")
      .split("\n")
      .map((l) => l.trim().split(/\s+/))
      .find(([, name]) => name === asset || name === `*${asset}`)?.[0];
    if (!expected) {
      throw new Error(`${asset} is not listed in checksums.txt`);
    }
    if (actual !== expected) {
      throw new Error(`checksum mismatch for ${asset}: got ${actual}, expected ${expected}`);
    }

    fs.mkdirSync(binDir, { recursive: true });
    extractBinary(archive, inner, platform.ext);
    // No mode bits on Windows; chmod there would be a no-op at best.
    let installed = binPath;
    if (process.platform !== "win32") {
      fs.chmodSync(binPath, 0o755);
      installed = putBinaryWhereTheLinkPoints();
    }
    console.log(`recall: installed ${installed}`);
  } catch (err) {
    fail(
      `could not install the binary: ${err.message}\n` +
        `  If no release exists for v${version} yet, install another way:\n` +
        `    cargo install --git https://github.com/${REPO} recall\n` +
        `    brew tap pimlabs/recall https://github.com/${REPO} && brew install --HEAD pimlabs/recall/recall`
    );
  }
}

main();
