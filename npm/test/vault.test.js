"use strict";

const assert = require("node:assert/strict");
const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const test = require("node:test");

const wrapper = path.resolve(__dirname, "../bin/vault.js");
const { localBinary, targetTriple } = require(wrapper);

test("maps every published platform to its Rust target", () => {
  assert.equal(targetTriple("linux", "x64"), "x86_64-unknown-linux-musl");
  assert.equal(targetTriple("linux", "arm64"), "aarch64-unknown-linux-musl");
  assert.equal(targetTriple("darwin", "x64"), "x86_64-apple-darwin");
  assert.equal(targetTriple("darwin", "arm64"), "aarch64-apple-darwin");
  assert.throws(() => targetTriple("win32", "x64"), /no prebuilt binary/);
});

test("validates the local binary override", (t) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "vault-wrapper-test-"));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const bin = path.join(dir, "vault");
  fs.writeFileSync(bin, "#!/bin/sh\nexit 0\n", { mode: 0o755 });

  assert.equal(localBinary(bin), bin);
  assert.throws(
    () => localBinary(path.join(dir, "missing")),
    /VAULT_BINARY is not executable/
  );
});

test("the wrapper forwards arguments and the child exit code", (t) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "vault-wrapper-test-"));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const fake = path.join(dir, "fake-vault.js");
  const argsFile = path.join(dir, "args.json");
  fs.writeFileSync(
    fake,
    "#!/usr/bin/env node\n" +
      'require("node:fs").writeFileSync(process.env.VAULT_ARGS_FILE, JSON.stringify(process.argv.slice(2)));\n' +
      "process.exit(Number(process.env.VAULT_FAKE_EXIT));\n",
    { mode: 0o755 }
  );

  const result = spawnSync(
    process.execPath,
    [wrapper, "--run", "tests/http-only", "--tag", "smoke"],
    {
      encoding: "utf8",
      env: {
        ...process.env,
        VAULT_BINARY: fake,
        VAULT_ARGS_FILE: argsFile,
        VAULT_FAKE_EXIT: "7",
      },
    }
  );

  assert.equal(result.status, 7, result.stderr);
  assert.deepEqual(JSON.parse(fs.readFileSync(argsFile, "utf8")), [
    "--run",
    "tests/http-only",
    "--tag",
    "smoke",
  ]);
});
