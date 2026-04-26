#!/usr/bin/env node
/**
 * Subprocess test for hello.mjs — exercises the stdin/stdout JSON-RPC
 * contract without the Smiths-Net engine.
 */

import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const __dirname = dirname(fileURLToPath(import.meta.url));
const PLUGIN = join(__dirname, "hello.mjs");

let passed = 0;
let failed = 0;

/**
 * Send a JSON-RPC request and read the response.
 * @param {object} request
 * @returns {Promise<object>}
 */
function rpc(proc, request) {
  return new Promise((resolve, reject) => {
    let buffer = "";

    const onData = (chunk) => {
      buffer += chunk.toString();
      const lines = buffer.split("\n");
      for (const line of lines) {
        const trimmed = line.trim();
        if (!trimmed) continue;
        try {
          const response = JSON.parse(trimmed);
          proc.stdout.removeListener("data", onData);
          resolve(response);
          return;
        } catch {
          // partial line, keep buffering
        }
      }
    };

    proc.stdout.on("data", onData);
    proc.stdin.write(JSON.stringify(request) + "\n");

    setTimeout(() => reject(new Error("timeout")), 3000);
  });
}

function assert(condition, label) {
  if (condition) {
    console.log(`✓ ${label}`);
    passed++;
  } else {
    console.error(`✗ ${label}`);
    failed++;
  }
}

async function main() {
  const proc = spawn("node", [PLUGIN], {
    stdio: ["pipe", "pipe", "pipe"],
  });

  // Test 1: describe_capabilities
  const desc = await rpc(proc, {
    jsonrpc: "2.0",
    id: 1,
    method: "describe_capabilities",
    params: {},
  });
  assert(desc.result?.name === "hello-world-nodejs", "describe_capabilities");

  // Test 2: echo unknown method
  const echo = await rpc(proc, {
    jsonrpc: "2.0",
    id: 2,
    method: "ping",
    params: {},
  });
  assert(echo.result?.echo === "ping", "echo unknown method");

  // Test 3: shutdown
  const shut = await rpc(proc, {
    jsonrpc: "2.0",
    id: 3,
    method: "shutdown",
    params: {},
  });
  assert(shut.result?.ok === true, "shutdown");

  // Wait for process to exit.
  await new Promise((resolve) => proc.on("close", resolve));

  console.log(`\n${passed} passed, ${failed} failed`);
  process.exit(failed > 0 ? 1 : 0);
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
