#!/usr/bin/env node
/**
 * Minimal sidecar plugin for Smiths-Net (Node.js).
 *
 * Speaks JSON-RPC 2.0 over stdin/stdout (newline-delimited).
 * The engine sends requests; this script replies.
 *
 * Protocol:
 *   Engine → Plugin:  {"jsonrpc":"2.0","id":1,"method":"...","params":{}}
 *   Plugin → Engine:  {"jsonrpc":"2.0","id":1,"result":{...}}
 */

import { createInterface } from "node:readline";

/**
 * Send one JSON-RPC frame to stdout.
 * @param {object} obj
 */
function send(obj) {
  process.stdout.write(JSON.stringify(obj) + "\n");
}

/**
 * Send a success response.
 * @param {number} id
 * @param {any} result
 */
function respond(id, result) {
  send({ jsonrpc: "2.0", id, result });
}

/**
 * Dispatch one incoming JSON-RPC request.
 * @param {object} request
 */
function handle(request) {
  const { id, method } = request;

  switch (method) {
    case "describe_capabilities":
      respond(id, {
        name: "hello-world-nodejs",
        version: "0.1.0",
        capabilities: [],
      });
      break;

    case "shutdown":
      respond(id, { ok: true });
      process.exit(0);
      break;

    default:
      // Echo the method name back — proof the plugin is alive.
      respond(id, { echo: method });
  }
}

// Main: read JSON-RPC requests from stdin, dispatch each one.
const rl = createInterface({ input: process.stdin });

rl.on("line", (line) => {
  const trimmed = line.trim();
  if (!trimmed) return;

  try {
    const request = JSON.parse(trimmed);
    handle(request);
  } catch {
    process.stderr.write(`bad frame: ${trimmed}\n`);
  }
});
