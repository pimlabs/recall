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

// `target` names the release asset (`recall_<target>.<ext>`). `exeName` is
// the file the archive holds *inside* it: the Unix tar.gz archives rename
// the binary to `recall_<target>` (see release.yml's Package (Unix) step),
// while the Windows zip keeps the bare `recall.exe` (Package (Windows)) —
// two conventions from two packaging steps, so this table carries both
// rather than assuming one.
const PLATFORMS = {
  "darwin:x64": { target: "darwin_amd64", ext: "tar.gz", exeName: "recall_darwin_amd64" },
  "darwin:arm64": { target: "darwin_arm64", ext: "tar.gz", exeName: "recall_darwin_arm64" },
  "linux:x64": { target: "linux_amd64", ext: "tar.gz", exeName: "recall_linux_amd64" },
  "linux:arm64": { target: "linux_arm64", ext: "tar.gz", exeName: "recall_linux_arm64" },
  "win32:x64": { target: "windows_amd64", ext: "zip", exeName: "recall.exe" },
  "win32:arm64": { target: "windows_arm64", ext: "zip", exeName: "recall.exe" },
};

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

// Every archive holds exactly one file, tar.gz or zip. Rather than pull in a
// library for either, shell out to the `tar` every supported platform
// already has — including Windows, where the built-in tar.exe (bsdtar, via
// libarchive) has shipped since Windows 10 and reads zip just as well as
// gzip, auto-detecting the format from the file's own bytes rather than its
// name.
function extractSingleFile(archive, destDir, expectedName, ext) {
  const isZip = ext === "zip";
  const archivePath = path.join(destDir, isZip ? "recall.zip" : "recall.tar");
  fs.writeFileSync(archivePath, isZip ? archive : zlib.gunzipSync(archive));
  execFileSync("tar", ["-xf", archivePath, "-C", destDir]);
  fs.unlinkSync(archivePath);
  const extracted = path.join(destDir, expectedName);
  if (!fs.existsSync(extracted)) {
    throw new Error(`archive did not contain ${expectedName}`);
  }
  return extracted;
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
  const { target, ext, exeName } = platform;

  const asset = `recall_${target}.${ext}`;
  const base = `https://github.com/${REPO}/releases/download/v${version}`;

  try {
    const [archive, checksums] = await Promise.all([
      download(`${base}/${asset}`),
      download(`${base}/checksums.txt`),
    ]);

    // Never make something usable that hasn't been checked against the
    // release's own manifest.
    const actual = crypto.createHash("sha256").update(archive).digest("hex");
    const line = checksums
      .toString("utf8")
      .split("\n")
      .find((l) => l.trim().endsWith(asset));
    if (!line) {
      throw new Error(`${asset} is not listed in checksums.txt`);
    }
    const expected = line.trim().split(/\s+/)[0];
    if (actual !== expected) {
      throw new Error(`checksum mismatch for ${asset}: got ${actual}, expected ${expected}`);
    }

    fs.mkdirSync(binDir, { recursive: true });
    const extracted = extractSingleFile(archive, binDir, exeName, ext);
    fs.renameSync(extracted, binPath);
    // No mode bits on Windows; chmod there would be a no-op at best.
    if (process.platform !== "win32") {
      fs.chmodSync(binPath, 0o755);
    }
    console.log(`recall: installed ${binPath}`);
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
