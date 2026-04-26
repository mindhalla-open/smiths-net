#!/usr/bin/env node
/**
 * Subprocess test for canonical_hook.mjs — exercises the JSON-RPC
 * contract without the Smiths-Net engine.
 */

import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const __dirname = dirname(fileURLToPath(import.meta.url));
const PLUGIN = join(__dirname, "canonical_hook.mjs");

let passed = 0;
let failed = 0;

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
          // partial line
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
  assert(desc.result?.name === "canonical-hook-nodejs", "describe_capabilities");
  assert(desc.result?.capabilities?.length === 1, "one capability");

  // Test 2: invoke ai.summarize
  const sum = await rpc(proc, {
    jsonrpc: "2.0",
    id: 2,
    method: "invoke",
    params: {
      tool: "ai.summarize",
      args: { text: "Hello, world! This is a test string for summarization.", max_length: 20 },
    },
  });
  assert(sum.result?.summary?.length <= 21, "invoke ai.summarize (truncated)");

  // Test 3: invoke ai.summarize with short text (no truncation)
  const short = await rpc(proc, {
    jsonrpc: "2.0",
    id: 3,
    method: "invoke",
    params: {
      tool: "ai.summarize",
      args: { text: "Short." },
    },
  });
  assert(short.result?.summary === "Short.", "invoke ai.summarize (short, no truncation)");

  // Test 4: invoke unknown tool → error
  const unk = await rpc(proc, {
    jsonrpc: "2.0",
    id: 4,
    method: "invoke",
    params: { tool: "unknown.tool", args: {} },
  });
  assert(unk.error?.code === -32601, "invoke unknown tool → error");

  // Test 5: missing text param → error
  const miss = await rpc(proc, {
    jsonrpc: "2.0",
    id: 5,
    method: "invoke",
    params: { tool: "ai.summarize", args: {} },
  });
  assert(miss.error?.code === -32602, "invoke missing text → error");

  // Test 6: unknown method → error
  const bad = await rpc(proc, {
    jsonrpc: "2.0",
    id: 6,
    method: "nonsense",
    params: {},
  });
  assert(bad.error?.code === -32601, "unknown method → error");

  // Test 7: shutdown
  const shut = await rpc(proc, {
    jsonrpc: "2.0",
    id: 7,
    method: "shutdown",
    params: {},
  });
  assert(shut.result?.ok === true, "shutdown");

  await new Promise((resolve) => proc.on("close", resolve));

  console.log(`\n${passed} passed, ${failed} failed`);
  process.exit(failed > 0 ? 1 : 0);
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
