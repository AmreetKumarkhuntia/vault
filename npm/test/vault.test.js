"use strict";

const assert = require("node:assert/strict");
const { spawn, spawnSync } = require("node:child_process");
const crypto = require("node:crypto");
const fs = require("node:fs");
const http = require("node:http");
const os = require("node:os");
const path = require("node:path");
const test = require("node:test");

const wrapper = path.resolve(__dirname, "../bin/vault.js");
const { fetchBytes, installTarball, localBinary, targetTriple } = require(wrapper);

function tmpdir(t) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "vault-wrapper-test-"));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  return dir;
}

// Builds a release tarball the way scripts/package-release.sh lays it out:
// <name>/vault inside <name>.tar.gz, plus the matching sha256sum line.
function release(t, { tag = "v9.9.9", triple = "x86_64-unknown-linux-musl" } = {}) {
  const work = tmpdir(t);
  const name = `vault-${tag}-${triple}`;
  fs.mkdirSync(path.join(work, name));
  fs.writeFileSync(path.join(work, name, "vault"), "#!/bin/sh\necho ok\n", { mode: 0o755 });
  const tar = spawnSync("tar", ["-czf", `${name}.tar.gz`, name], { cwd: work });
  assert.equal(tar.status, 0, String(tar.stderr));
  const tarball = fs.readFileSync(path.join(work, `${name}.tar.gz`));
  const sha = crypto.createHash("sha256").update(tarball).digest("hex");
  return { tarball, sha, asset: `${name}.tar.gz`, sums: `${sha}  ${name}.tar.gz\n`, tag, triple };
}

test("maps every published platform to its Rust target", () => {
  assert.equal(targetTriple("linux", "x64"), "x86_64-unknown-linux-musl");
  assert.equal(targetTriple("linux", "arm64"), "aarch64-unknown-linux-musl");
  assert.equal(targetTriple("darwin", "x64"), "x86_64-apple-darwin");
  assert.equal(targetTriple("darwin", "arm64"), "aarch64-apple-darwin");
  assert.throws(() => targetTriple("win32", "x64"), /no prebuilt binary/);
});

test("validates the local binary override", (t) => {
  const dir = tmpdir(t);
  const bin = path.join(dir, "vault");
  fs.writeFileSync(bin, "#!/bin/sh\nexit 0\n", { mode: 0o755 });

  assert.equal(localBinary(bin), bin);
  assert.throws(
    () => localBinary(path.join(dir, "missing")),
    /VAULT_BINARY is not executable/
  );
});

test("the wrapper forwards arguments, color environment, and the child exit code", (t) => {
  const dir = tmpdir(t);
  const fake = path.join(dir, "fake-vault.js");
  const argsFile = path.join(dir, "args.json");
  fs.writeFileSync(
    fake,
    "#!/usr/bin/env node\n" +
      'require("node:fs").writeFileSync(process.env.VAULT_ARGS_FILE, JSON.stringify({ args: process.argv.slice(2), noColor: process.env.NO_COLOR }));\n' +
      "process.exit(Number(process.env.VAULT_FAKE_EXIT));\n",
    { mode: 0o755 }
  );

  const result = spawnSync(
    process.execPath,
    [wrapper, "--no-color", "--run", "tests/http-only", "--tag", "smoke"],
    {
      encoding: "utf8",
      env: {
        ...process.env,
        VAULT_BINARY: fake,
        VAULT_ARGS_FILE: argsFile,
        VAULT_FAKE_EXIT: "7",
        NO_COLOR: "1",
      },
    }
  );

  assert.equal(result.status, 7, result.stderr);
  assert.deepEqual(JSON.parse(fs.readFileSync(argsFile, "utf8")), {
    args: ["--no-color", "--run", "tests/http-only", "--tag", "smoke"],
    noColor: "1",
  });
});

test("a signal-killed binary ends the wrapper with that signal, not exit 0", (t) => {
  const dir = tmpdir(t);
  const fake = path.join(dir, "fake-vault.js");
  fs.writeFileSync(fake, '#!/usr/bin/env node\nprocess.kill(process.pid, "SIGTERM");\n', {
    mode: 0o755,
  });

  const result = spawnSync(process.execPath, [wrapper], {
    encoding: "utf8",
    env: { ...process.env, VAULT_BINARY: fake },
  });

  assert.ok(
    result.signal === "SIGTERM" || result.status === 143,
    `expected SIGTERM/143, got status=${result.status} signal=${result.signal} ${result.stderr}`
  );
});

test("installs a verified tarball as a single executable and leaves no stage behind", (t) => {
  const rel = release(t);
  const dir = tmpdir(t);

  installTarball(rel.tarball, rel.sums, { tag: rel.tag, triple: rel.triple, dir });

  assert.deepEqual(fs.readdirSync(dir), ["vault"]);
  fs.accessSync(path.join(dir, "vault"), fs.constants.X_OK);
  const run = spawnSync(path.join(dir, "vault"), { encoding: "utf8" });
  assert.equal(run.stdout, "ok\n");
});

test("rejects a tampered tarball, a missing entry, and a suffix-only name match", (t) => {
  const rel = release(t);
  const dir = tmpdir(t);
  const opts = { tag: rel.tag, triple: rel.triple, dir };

  assert.throws(
    () => installTarball(Buffer.concat([rel.tarball, Buffer.from("x")]), rel.sums, opts),
    /sha256 mismatch/
  );
  assert.throws(() => installTarball(rel.tarball, "", opts), /not listed in SHA256SUMS/);
  assert.throws(
    () => installTarball(rel.tarball, `${rel.sha}  evil-${rel.asset}\n`, opts),
    /not listed in SHA256SUMS/
  );
  // sha256sum's binary-mode marker is still an exact name match.
  installTarball(rel.tarball, `${rel.sha} *${rel.asset}\n`, opts);
  assert.deepEqual(fs.readdirSync(dir), ["vault"]);
});

test("a failed extraction leaves neither a binary nor a stage directory", (t) => {
  const rel = release(t);
  const dir = tmpdir(t);
  const junk = Buffer.from("this is not a tarball");
  const sums = `${crypto.createHash("sha256").update(junk).digest("hex")}  ${rel.asset}\n`;

  assert.throws(
    () => installTarball(junk, sums, { tag: rel.tag, triple: rel.triple, dir }),
    /tar extraction failed/
  );
  assert.deepEqual(fs.readdirSync(dir), []);
});

test("concurrent installs into one cache directory do not corrupt each other", async (t) => {
  const rel = release(t);
  const dir = tmpdir(t);
  const work = tmpdir(t);
  fs.writeFileSync(path.join(work, "pkg.tgz"), rel.tarball);
  fs.writeFileSync(path.join(work, "SUMS"), rel.sums);
  const script =
    `const fs = require("node:fs"); const { installTarball } = require(${JSON.stringify(wrapper)});` +
    `installTarball(fs.readFileSync(${JSON.stringify(path.join(work, "pkg.tgz"))}),` +
    ` fs.readFileSync(${JSON.stringify(path.join(work, "SUMS"))}, "utf8"),` +
    ` { tag: ${JSON.stringify(rel.tag)}, triple: ${JSON.stringify(rel.triple)}, dir: ${JSON.stringify(dir)} });`;

  const exits = await Promise.all(
    [1, 2].map(
      () =>
        new Promise((resolve) => {
          const child = spawn(process.execPath, ["-e", script], { stdio: ["ignore", "ignore", "pipe"] });
          let stderr = "";
          child.stderr.on("data", (d) => (stderr += d));
          child.on("exit", (code) => resolve({ code, stderr }));
        })
    )
  );

  for (const e of exits) assert.equal(e.code, 0, e.stderr);
  assert.deepEqual(fs.readdirSync(dir), ["vault"]);
  fs.accessSync(path.join(dir, "vault"), fs.constants.X_OK);
});

test("a stalled download is abandoned after the timeout", async (t) => {
  const server = http.createServer(() => {
    // never respond
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(() => server.close());
  const url = `http://127.0.0.1:${server.address().port}/vault.tar.gz`;

  await assert.rejects(fetchBytes(url, 100), (e) => e.name === "TimeoutError");
});
