#!/usr/bin/env node
"use strict";
const { spawn, spawnSync } = require("child_process");
const crypto = require("crypto");
const fs = require("fs");
const os = require("os");
const path = require("path");

const pkg = require("../package.json");
const REPO = "AmreetKumarkhuntia/vault";

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
        path.join(os.homedir(), ".cache", "amreetkumarkhuntia-vault"),
      pkg.version,
      triple
    );
    fs.mkdirSync(fallback, { recursive: true });
    return fallback;
  }
}

async function fetchBytes(url) {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`GET ${url} -> ${res.status}`);
  return Buffer.from(await res.arrayBuffer());
}

async function download(triple, dir) {
  const tag = `v${pkg.version}`;
  const asset = `vault-${tag}-${triple}.tar.gz`;
  const base = `https://github.com/${REPO}/releases/download/${tag}`;
  const [tarball, sums] = await Promise.all([
    fetchBytes(`${base}/${asset}`),
    fetchBytes(`${base}/SHA256SUMS`).then((b) => b.toString("utf8")),
  ]);
  const line = sums.split("\n").find((l) => l.trim().endsWith(asset));
  if (!line) throw new Error(`${asset} not listed in SHA256SUMS`);
  const expected = line.trim().split(/\s+/)[0];
  const actual = crypto.createHash("sha256").update(tarball).digest("hex");
  if (actual !== expected) {
    throw new Error(
      `sha256 mismatch for ${asset}: expected ${expected}, got ${actual}`
    );
  }
  const tmp = path.join(dir, `.${asset}.tmp`);
  fs.writeFileSync(tmp, tarball);
  const r = spawnSync(
    "tar",
    ["-xzf", tmp, "--strip-components=1", "-C", dir, `vault-${tag}-${triple}/vault`],
    { stdio: "inherit" }
  );
  fs.unlinkSync(tmp);
  if (r.status !== 0) throw new Error("tar extraction failed");
  fs.chmodSync(path.join(dir, "vault"), 0o755);
}

function launch(bin) {
  const child = spawn(bin, process.argv.slice(2), { stdio: "inherit" });
  for (const sig of ["SIGINT", "SIGTERM", "SIGHUP"]) {
    process.on(sig, () => child.kill(sig));
  }
  child.on("exit", (code, signal) => {
    // Re-raise fatal signals so the shell sees the true termination cause.
    if (signal) process.kill(process.pid, signal);
    else process.exit(code == null ? 1 : code);
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
    console.error(`vault: downloading v${pkg.version} (${triple})...`);
    await download(triple, dir);
  }
  launch(bin);
}

if (require.main === module) {
  main().catch((e) => fail(e.message));
}

module.exports = { localBinary, targetTriple };
