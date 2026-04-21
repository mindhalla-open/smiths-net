# dialplan-yaml

Reference `routing.dialplan` sidecar that reads rules from a YAML
file. Sits alongside `route-rhai` — same `AiProvider` seam, same
capability, different authoring style. Pick YAML for "just move
these URIs to those pools on these hours"; pick Rhai for richer
logic (LLM-routed tags, time-zone math, external KV lookups).

## Quick start

```bash
# Edit the shipped example rules — same dir as this README.
$EDITOR rules.yaml

# Drop this directory under plugins.dir and start smiths-net.
# list_ai_providers shows dialplan-yaml alongside route-rhai.
```

## Rule shape

```yaml
rules:
  - match:
      from: "regex"              # Python re.search() on the From URI
      to:   "regex"              # Python re.search() on the To URI
      hour_range: [start, end]   # local-time hour window, [start, end)
    rewrite:
      target: "sip:dest@host"
      reason: "free-form, appears in structuredContent"
```

Every `match` field is optional (absent = matches). Rules evaluate
top-to-bottom; first match wins. No match → empty response → the
engine falls through to its default routing.

## Reload

Two paths:

* `reload_plugin` MCP tool respawns the whole sidecar — picks up
  `main.py` edits (rarely needed).
* Custom `reload_rules` RPC method reloads just `rules.yaml`
  without restarting the process. Operators can wire it into an
  editor save-hook or a Git-pull CI job.

## Stdlib-only

No `PyYAML` dependency. Ships a tiny YAML-subset parser that
handles mappings, sequences, inline flow lists (`[a, b]`),
quoted/bare strings, ints, booleans, and `#` comments — enough for
the rule file shape. If you need full YAML 1.2 (anchors, tags,
multi-doc streams), swap the `loads` call in `main.py` for
`import yaml; yaml.safe_load`.

## Priority

Advertises `priority = 40` so it sits between the reference Rhai
dialplan (`route-rhai`, `priority = 50`) and a hypothetical
externally-hosted routing service. Flip the order by editing the
descriptor if your deployment prefers YAML.
