#!/usr/bin/env node
"use strict";

const fs = require("node:fs");
const http = require("node:http");

const server = http.createServer((req, res) => {
  if (req.method === "GET" && req.url === "/healthz") {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ status: "ok" }));
    return;
  }
  res.writeHead(404, { "content-type": "application/json" });
  res.end(JSON.stringify({ error: "not found" }));
});

server.listen(0, "127.0.0.1", () => {
  const address = server.address();
  if (!address || typeof address === "string") {
    throw new Error("HTTP fixture did not bind a TCP port");
  }
  if (process.env.VAULT_HTTP_PORT_FILE) {
    fs.writeFileSync(process.env.VAULT_HTTP_PORT_FILE, String(address.port));
  }
  console.log(`HTTP fixture listening on http://127.0.0.1:${address.port}`);
});

for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => server.close(() => process.exit(0)));
}
