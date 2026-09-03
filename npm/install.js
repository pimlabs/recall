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
const binPath = path.join(binDir, "recall-bin");

const PLATFORMS = {
  "darwin:x64": "darwin_amd64",
  "darwin:arm64": "darwin_arm64",
  "linux:x64": "linux_amd64",
  "linux:arm64": "linux_arm64",
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

// The tarball holds exactly one file. Rather than pull in a tar library for
// that, shell out to the tar every supported platform already has.
function extractSingleFile(tarGz, destDir, expectedName) {
  const tarPath = path.join(destDir, "recall.tar");
  fs.writeFileSync(tarPath, zlib.gunzipSync(tarGz));
  execFileSync("tar", ["-xf", tarPath, "-C", destDir]);
  fs.unlinkSync(tarPath);
  const extracted = path.join(destDir, expectedName);
  if (!fs.existsSync(extracted)) {
    throw new Error(`archive did not contain ${expectedName}`);
  }
  return extracted;
}

async function main() {
  const key = `${process.platform}:${process.arch}`;
  const target = PLATFORMS[key];
  if (!target) {
    fail(
      `unsupported platform ${key}. macOS and Linux on x64/arm64 only; ` +
        `Windows needs WSL. See https://github.com/${REPO}`
    );
  }

  const asset = `recall_${target}.tar.gz`;
  const base = `https://github.com/${REPO}/releases/download/v${version}`;

  try {
    const [tarGz, checksums] = await Promise.all([
      download(`${base}/${asset}`),
      download(`${base}/checksums.txt`),
    ]);

    // Never make something executable that hasn't been checked against the
    // release's own manifest.
    const actual = crypto.createHash("sha256").update(tarGz).digest("hex");
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
    const extracted = extractSingleFile(tarGz, binDir, `recall_${target}`);
    fs.renameSync(extracted, binPath);
    fs.chmodSync(binPath, 0o755);
    console.log(`recall: installed ${binPath}`);
  } catch (err) {
    fail(
      `could not install the binary: ${err.message}\n` +
        `  If no release exists for v${version} yet, install another way:\n` +
        `    brew tap pimlabs/recall https://github.com/${REPO} && brew install --HEAD pimlabs/recall/recall\n` +
        `    or build from source: go build -o recall ./cmd/recall`
    );
  }
}

main();
