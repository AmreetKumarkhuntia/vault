#!/usr/bin/env node
"use strict";
const { spawn, spawnSync } = require("child_process");
const crypto = require("crypto");
const fs = require("fs");
const os = require("os");
const path = require("path");

const pkg = require("../package.json");
const REPO = "AmreetKumarkhuntia/vault";
const SIGNALS = ["SIGINT", "SIGTERM", "SIGHUP"];

const TARGETS = {
  "linux-x64": "x86_64-unknown-linux-musl",
  "linux-arm64": "aarch64-unknown-linux-musl",
  "darwin-x64": "x86_64-apple-darwin",
  "darwin-arm64": "aarch64-apple-darwin",
};

function fail(msg) {
  console.error(`vault: ${msg}`);
  process.exit(1);
}

function targetTriple(platform = process.platform, arch = process.arch) {
  const key = `${platform}-${arch}`;
  const triple = TARGETS[key];
  if (!triple) {
    throw new Error(
      `no prebuilt binary for ${key}. Prebuilt binaries cover Linux and macOS x64/arm64.\n` +
        `Build from source instead:\n` +
        `  cargo install --git https://github.com/${REPO} vault\n` +
        `or: git clone https://github.com/${REPO} && cd vault && make build`
    );
  }
  return triple;
}

function localBinary(configured = process.env.VAULT_BINARY) {
  if (!configured) return null;
  const bin = path.resolve(configured);
  try {
    const stat = fs.statSync(bin);
    if (!stat.isFile()) throw new Error("not a regular file");
    fs.accessSync(bin, fs.constants.X_OK);
  } catch (e) {
    throw new Error(`VAULT_BINARY is not executable (${bin}): ${e.message}`);
  }
  return bin;
}

// Package dir first; falls back to ~/.cache when the package dir is
// read-only (root-owned global installs).
function cacheDir(triple) {
  const primary = path.join(__dirname, ".cache", pkg.version, triple);
  try {
    fs.mkdirSync(primary, { recursive: true });
    fs.accessSync(path.dirname(primary), fs.constants.W_OK);
    return primary;
  } catch {
    const fallback = path.join(
      process.env.VAULT_DOWNLOAD_DIR ||
        path.join(os.homedir(), ".cache", "thunderkiller-vault"),
      pkg.version,
      triple
    );
    fs.mkdirSync(fallback, { recursive: true });
    return fallback;
  }
}

async function fetchBytes(url, timeoutMs = Number(process.env.VAULT_DOWNLOAD_TIMEOUT_MS) || 60_000) {
  let res;
  try {
    res = await fetch(url, { signal: AbortSignal.timeout(timeoutMs) });
  } catch (e) {
    if (e.name === "TimeoutError") {
      const err = new Error(`download of ${url} timed out after ${timeoutMs / 1000}s (set VAULT_DOWNLOAD_TIMEOUT_MS to allow longer)`);
      err.name = e.name;
      throw err;
    }
    throw e;
  }
  if (!res.ok) throw new Error(`GET ${url} -> ${res.status}`);
  return Buffer.from(await res.arrayBuffer());
}

// SHA256SUMS is bundled into the npm package at publish time, so a swapped
// release asset cannot bring its own matching checksum.
function bundledSums() {
  try {
    return fs.readFileSync(path.join(__dirname, "SHA256SUMS"), "utf8");
  } catch (e) {
    throw new Error(
      `bin/SHA256SUMS is missing from this package, so the download cannot be verified (${e.message}). ` +
        `Set VAULT_BINARY to a local build instead.`
    );
  }
}

function installTarball(tarball, sumsText, { tag, triple, dir }) {
  const name = `vault-${tag}-${triple}`;
  const asset = `${name}.tar.gz`;
  const entry = sumsText
    .split("\n")
    .map((l) => l.trim().split(/\s+/))
    .find(([, file]) => file && file.replace(/^\*/, "") === asset);
  if (!entry) throw new Error(`${asset} not listed in SHA256SUMS`);
  const expected = entry[0];
  const actual = crypto.createHash("sha256").update(tarball).digest("hex");
  if (actual !== expected) {
    throw new Error(
      `sha256 mismatch for ${asset}: expected ${expected}, got ${actual}`
    );
  }
  // Extract into a private stage and rename into place, so concurrent or
  // interrupted installs never leave a half-written binary at the final path.
  const stage = fs.mkdtempSync(path.join(dir, ".stage-"));
  try {
    fs.writeFileSync(path.join(stage, asset), tarball);
    const r = spawnSync(
      "tar",
      ["-xzf", asset, "--strip-components=1", `${name}/vault`],
      { cwd: stage, stdio: ["ignore", "ignore", "inherit"] }
    );
    if (r.status !== 0) {
      throw new Error(`tar extraction failed${r.error ? `: ${r.error.message}` : ""}`);
    }
    fs.chmodSync(path.join(stage, "vault"), 0o755);
    fs.renameSync(path.join(stage, "vault"), path.join(dir, "vault"));
  } finally {
    fs.rmSync(stage, { recursive: true, force: true });
  }
}

function launch(bin) {
  const child = spawn(bin, process.argv.slice(2), { stdio: "inherit" });
  for (const sig of SIGNALS) {
    process.on(sig, () => child.kill(sig));
  }
  child.on("exit", (code, signal) => {
    if (signal) {
      // Our forwarding handlers would catch the re-raised signal and exit 0.
      for (const sig of SIGNALS) process.removeAllListeners(sig);
      process.kill(process.pid, signal);
    } else {
      process.exit(code == null ? 1 : code);
    }
  });
  child.on("error", (e) => fail(`failed to exec downloaded binary: ${e.message}`));
}

async function main() {
  const override = localBinary();
  if (override) {
    launch(override);
    return;
  }
  if (pkg.version === "0.0.0") {
    throw new Error("this is an unpublished placeholder build; the version is stamped at publish time");
  }
  const triple = targetTriple();
  const dir = cacheDir(triple);
  const bin = path.join(dir, "vault");
  if (!fs.existsSync(bin)) {
    const sums = bundledSums();
    const tag = `v${pkg.version}`;
    console.error(`vault: downloading ${tag} (${triple})...`);
    const url = `https://github.com/${REPO}/releases/download/${tag}/vault-${tag}-${triple}.tar.gz`;
    installTarball(await fetchBytes(url), sums, { tag, triple, dir });
  }
  launch(bin);
}

if (require.main === module) {
  main().catch((e) =>
    fail(e.cause ? `${e.message}: ${e.cause.code || e.cause.message}` : e.message)
  );
}

module.exports = { fetchBytes, installTarball, localBinary, targetTriple };
