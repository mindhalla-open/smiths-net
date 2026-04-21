# route-rhai

Reference Rhai dialplan — slice 4.1 / P24. Demonstrates the
script-tier plugin ABI end-to-end.

## What it does

* Compiles once at boot via `smiths_script::ScriptRuntime`.
* `describe_capabilities()` advertises the `routing.dialplan`
  capability; the engine validates it against the plugin
  manifest's `provides` list.
* `route(req)` takes a call request (`from`, `to`, `call_id`, ...)
  and returns either `#{}` (fall through to engine default) or
  `#{ target: "sip:...", reason: "..." }`.

## Hot reload

Save `main.rhai`. The engine's `smiths_plugin::watcher` detects
the change, drains the old compiled AST, and swaps in the new
one on the next invocation. Rollback kicks in after
`ROLLBACK_AFTER = 5` consecutive errors — a stacktrace-spewing
script won't eat an entire worker slice.

## Budgets

`plugin.toml` caps each invocation at 200k ops and 50 ms wall
clock. Raise the numbers for a bigger lookup table; tighten them
for stricter isolation. Whichever limit trips first surfaces as a
`Budget` error the dispatcher fails over on.

## Testing a new script

Easiest iteration loop:

```bash
# From the repo root, with the engine running and plugins.dir
# pointed at `plugins/examples`.
vim plugins/examples/route-rhai/main.rhai
# Save — the watcher picks it up.
# Then either send a real INVITE or call the `route` method
# directly from the MCP adapter:
echo '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"put_script","arguments":{"name":"route-rhai","source":"...","engine":"rhai"}}}'
```

## Limits

* The script only has access to the request dict the engine
  hands it — no filesystem, no HTTP fetch, no subprocess. That's
  by design; scripts that need external I/O either call an
  `ai.*` sidecar (for LLM / embedding) or an engine host-function
  (for bus publish, KV read — those land when dialplan's Small 1
  lands alongside slice 4.2).
* Rhai is synchronous; the runtime runs every invocation on a
  `tokio::task::spawn_blocking` worker. Light scripts are fine;
  ~10 ms of Rhai work per call is a comfortable ceiling.
