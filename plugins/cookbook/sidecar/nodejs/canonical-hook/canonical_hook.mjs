#!/usr/bin/env node
/**
 * Canonical-hook sidecar plugin for Smiths-Net (Node.js).
 *
 * Speaks JSON-RPC 2.0 over stdin/stdout (newline-delimited).
 * Implements one tool: `ai.summarize` — truncates input text
 * to a configurable length with an ellipsis.
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
 * Send an error response.
 * @param {number} id
 * @param {number} code
 * @param {string} message
 */
function respondError(id, code, message) {
  send({ jsonrpc: "2.0", id, error: { code, message } });
}

/**
 * Tool: ai.summarize — truncate text to `max_length` chars.
 * @param {number} id
 * @param {object} params
 */
function invokeSummarize(id, params) {
  const text = params?.text;
  if (typeof text !== "string") {
    respondError(id, -32602, "missing required param: text");
    return;
  }

  const maxLength = params?.max_length ?? 100;
  if (typeof maxLength !== "number" || maxLength < 1) {
    respondError(id, -32602, "max_length must be a positive integer");
    return;
  }

  const summary =
    text.length <= maxLength ? text : text.slice(0, maxLength) + "…";

  respond(id, { summary, original_length: text.length });
}

/**
 * Dispatch one incoming JSON-RPC request.
 * @param {object} request
 */
function handle(request) {
  const { id, method, params = {} } = request;

  switch (method) {
    case "describe_capabilities":
      respond(id, {
        name: "canonical-hook-nodejs",
        version: "0.1.0",
        capabilities: [
          {
            capability: "ai.summarize",
            description:
              "Truncate text to a maximum length with an ellipsis.",
          },
        ],
      });
      break;

    case "invoke": {
      const tool = params?.tool;
      if (tool === "ai.summarize") {
        invokeSummarize(id, params?.args ?? {});
      } else {
        respondError(id, -32601, `unknown tool: ${tool}`);
      }
      break;
    }

    case "shutdown":
      respond(id, { ok: true });
      process.exit(0);
      break;

    default:
      respondError(id, -32601, `unknown method: ${method}`);
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
