# Tool authoring guide

smiths-net exposes its control plane as a set of typed `Tool`s.
One tool runs identically under every adapter the engine ships
(slice 4.4 / P20 counts three: MCP stdio, MCP HTTP, A2A HTTP, and
the new generic webhook). This guide walks through:

1. Where to add a tool.
2. The `Tool` trait contract.
3. How to plumb optional dependencies through `ToolContext`.
4. Errors + outcome classification.
5. How the same tool surfaces on every adapter.
6. Migration notes — what changed in 0.46.0.

## 1. Where tools live

Every tool lives in `crates/smiths-mcp/src/tools.rs` (or — when a
tool grows beyond ~100 LOC — its own module next to `tools.rs`).
Register it in `builtin_registry()` so all four adapters pick it
up automatically.

## 2. The `Tool` trait

```rust
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;                // stable wire id
    fn description(&self) -> &'static str;          // one sentence
    fn input_schema(&self) -> serde_json::Value;    // JSON Schema draft 2020-12
    async fn call(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<serde_json::Value, ToolError>;
}
```

Checklist for a new tool:

- **Name.** Stable, lowercase, snake_case. Treat the string like a
  public API; renaming breaks every pinned agent.
- **Description.** One sentence, 80 cols, describes *what* not
  *how*. Agents pick tools by reading this; be terse and concrete.
- **Input schema.** Use `serde_json::json!` + `draft 2020-12`
  shapes. Mark required fields; set `additionalProperties: false`
  so typos surface as validation errors.
- **Call.** Validate `args` against the schema manually (the
  registry doesn't). Return `Ok(serde_json::Value)` on success
  and one of the four `ToolError` variants otherwise — see §4.

## 3. `ToolContext` — optional subsystems

`ToolContext` holds the full set of engine handles — some present,
some `Option<>`:

| Field               | When present                                |
|---------------------|---------------------------------------------|
| `state`             | always                                      |
| `plugins`           | always (empty registry if none loaded)      |
| `config`            | always                                      |
| `media`             | always                                      |
| `metrics`           | when the CLI wired one                      |
| `originator`        | when SIP UAC is running                     |
| `registrations`    | when `[auth]` backend is live               |
| `cdr`               | when `[storage] backend` is configured      |
| `vector`            | when `[storage.vector]` is enabled          |
| `recording`         | when `[storage.recording]` is enabled       |
| `prompts`           | when `[media.prompts]` root is set          |

Return `ToolError::NotFound(msg)` with a pointer to the exact
config knob operators need to flip when a dependency is missing —
see `search_calls_semantic` or `record_prompt` for the canonical
shape.

## 4. Errors and outcomes

Four `ToolError` variants, each mapped to a stable status code
and JSON-RPC error number by `ControlOutcome`:

| `ToolError`          | HTTP | JSON-RPC | When                                         |
|----------------------|------|----------|----------------------------------------------|
| `InvalidArguments`   | 400  | -32602   | Schema mismatch, missing required field      |
| `NotFound`           | 404  | -32601   | Unknown call/plugin, missing config backend  |
| `Forbidden`          | 403  | -32001   | Rate-limit trip, auth denied                 |
| `Internal`           | 500  | -32603   | Unexpected failure the caller can't fix      |

Pick the narrowest variant that's true. Everything else is a
misclassification the agent can't act on.

## 5. Adapter matrix

Your tool becomes reachable under every adapter the moment you
register it:

| Adapter       | Path                           | Body shape                                   |
|---------------|--------------------------------|----------------------------------------------|
| MCP stdio     | stdin/stdout                   | `{"method":"tools/call","params":{"name","arguments"}}` |
| MCP HTTP      | `POST /mcp`                    | same as MCP stdio                            |
| A2A HTTP      | `POST /a2a`                    | same JSON-RPC shape                          |
| Webhook HTTP  | `POST /hook/<tool>`            | **just `args`**, no envelope                 |

The webhook adapter is the new one in 0.46.0. It's intended for
callers that don't speak MCP/A2A — Zapier, Make, bare `curl`,
Slack-slash-command webhooks. Success / error classification is
identical because all four adapters route through
`ProtocolDispatch::invoke`.

## 6. Migration — what changed in 0.46.0

**New public types:**

```rust
smiths_mcp::{
    ControlOutcome,       // Ok | InvalidArguments | NotFound | Forbidden | Internal
    ControlProtocol,      // trait: label + description + framing
    ProtocolDispatch,     // shared dispatch wrapper (registry+rl+metrics+ctx)
    McpStdioProtocol,     // singleton marker
    A2aHttpProtocol,      // singleton marker
    WebhookHttpProtocol,  // singleton marker
    agent_card,           // shared `.well-known/agent.json` builder
};
```

**No wire changes.** Existing agents that hit `/mcp` or `/a2a`
continue to work bit-for-bit. The new surface is additive — a
generic webhook adapter + the types that let operators plumb
custom adapters without re-deriving the dispatch pipeline.

**If you embed `smiths-mcp`** in a custom binary:

* Before: each adapter took `Arc<ToolRegistry> + Arc<RateLimiter>
  + Arc<Metrics> + ToolContext` separately. Still works.
* After: prefer building one `ProtocolDispatch` and cloning it
  into every adapter. Halves the argument count at the call
  site; makes adding new adapters a one-liner.

**If you just author tools** — nothing to migrate. The `Tool`
trait, `ToolContext`, and `ToolError` are byte-identical. The new
types are for adapter authors, not tool authors.

## See also

* `crates/smiths-mcp/src/tools.rs` — every built-in tool, read
  `TranslateTool`, `SearchCallsSemanticTool`, and `SummarizeCallTool`
  for the canonical shapes.
* `crates/smiths-mcp/src/control_protocol.rs` — the trait
  declarations and the `agent_card` helper.
* `crates/smiths-mcp/tests/a2a_make_call.rs` — end-to-end proof
  that one tool implementation works from an external HTTP
  client, with the stubbed originator + discovery probe.
