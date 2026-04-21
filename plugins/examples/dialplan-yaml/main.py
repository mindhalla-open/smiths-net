#!/usr/bin/env python3
"""YAML-driven dialplan sidecar — slice 4.2 / P9 reference.

Loads `rules.yaml` from the plugin's directory at boot, evaluates
each inbound `route(req)` against the rule list, returns the first
match as a `{target, reason}` rewrite. No match → `{}` → the engine
falls through to its default routing.

Rule shape (see `rules.yaml`):

    rules:
      - match:
          from: "regex"           # Python re.search() on From URI
          to:   "regex"           # Python re.search() on To URI
          hour_range: [start, end]  # local-time hour window,
                                     # [start, end)
        rewrite:
          target: "sip:dest@host[:port]"
          reason: "free-form"

Stdlib-only — includes a tiny YAML subset parser so operators don't
have to install PyYAML. The subset covers everything the example
rule file uses: mappings, sequences, inline lists (`[1, 2]`),
single/double-quoted strings, bare strings, ints, and `#` comments.
Operators who need the full YAML 1.2 spec can replace the `loads`
call with `import yaml; yaml.safe_load` in their own fork.
"""

from __future__ import annotations

import datetime
import json
import os
import re
import sys
from pathlib import Path

RULES_FILE = os.environ.get("DIALPLAN_RULES", "rules.yaml")

DESCRIPTOR = {
    "capability": "routing.dialplan",
    "plugin": "dialplan-yaml",
    "model_id": "yaml-rules-v1",
    "abi": "1.0",
    "description": "YAML-driven dialplan: from/to regex + hour_range matchers.",
    "priority": 40,
}


def reply(id_, *, result=None, error=None):
    frame = {"jsonrpc": "2.0"}
    if id_ is not None:
        frame["id"] = id_
    if error is not None:
        frame["error"] = error
    else:
        frame["result"] = result
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()


def log(msg: str) -> None:
    sys.stderr.write(f"[dialplan-yaml] {msg}\n")
    sys.stderr.flush()


# ---------------------------------------------------------------------
# Tiny YAML-subset parser. Covers mappings, sequences, inline flow
# lists (`[a, b]`), quoted/bare strings, ints, and `#` comments.
# ---------------------------------------------------------------------


def _strip_comment(line: str) -> str:
    # Only strip `#` that isn't inside a quoted string. The
    # rule-file shape doesn't put `#` in values, but the guard is
    # cheap enough to keep.
    out = []
    in_q = None
    for ch in line:
        if in_q:
            if ch == in_q:
                in_q = None
            out.append(ch)
        elif ch in ('"', "'"):
            in_q = ch
            out.append(ch)
        elif ch == "#":
            break
        else:
            out.append(ch)
    return "".join(out).rstrip()


def _scalar(raw: str):
    raw = raw.strip()
    if raw == "" or raw == "~" or raw.lower() == "null":
        return None
    if raw.lower() in ("true", "yes"):
        return True
    if raw.lower() in ("false", "no"):
        return False
    if (raw.startswith('"') and raw.endswith('"')) or (raw.startswith("'") and raw.endswith("'")):
        return raw[1:-1]
    try:
        return int(raw)
    except ValueError:
        pass
    try:
        return float(raw)
    except ValueError:
        pass
    if raw.startswith("[") and raw.endswith("]"):
        inner = raw[1:-1].strip()
        if not inner:
            return []
        return [_scalar(item) for item in _split_flow(inner)]
    return raw


def _split_flow(body: str) -> list[str]:
    """Split an inline flow list on top-level commas, respecting
    quotes and nested brackets. Enough for `[1, 2]` / `["a", "b"]` /
    `[[1,2], [3,4]]`."""
    depth = 0
    buf = []
    out = []
    in_q = None
    for ch in body:
        if in_q:
            buf.append(ch)
            if ch == in_q:
                in_q = None
        elif ch in ('"', "'"):
            in_q = ch
            buf.append(ch)
        elif ch == "[":
            depth += 1
            buf.append(ch)
        elif ch == "]":
            depth -= 1
            buf.append(ch)
        elif ch == "," and depth == 0:
            out.append("".join(buf))
            buf = []
        else:
            buf.append(ch)
    if buf:
        out.append("".join(buf))
    return out


def _parse_block(lines: list[tuple[int, str]], start: int, indent: int):
    """Recursive-descent block parser. Returns (value, next_index).

    `indent` is the minimum leading-space count a line must have to
    belong to this block. Children with equal or greater indent are
    consumed; anything less ends the block. For nested sub-blocks
    we anchor on the *first* child line's indent so a later sibling
    at the outer indent correctly terminates the inner block — that
    was the subtle bug that made `rewrite:` get pulled inside
    `match:` in the earlier draft."""
    if start >= len(lines):
        return None, start
    ind, first = lines[start]
    if first.startswith("- "):
        item_indent = ind
        out: list = []
        i = start
        while i < len(lines):
            cur_ind, cur = lines[i]
            if cur_ind < item_indent or not cur.startswith("- "):
                break
            if cur_ind > item_indent:
                break
            content = cur[2:]
            if ":" in content and not content.startswith("[") and not content.startswith("{"):
                key, _, rest = content.partition(":")
                rest = rest.strip()
                if rest:
                    elem: dict = {key.strip(): _scalar(rest)}
                    j = i + 1
                    # Sibling keys under this list item share
                    # the indent of the `<key>:` text inside the
                    # `- ` marker — which is item_indent + 2.
                    sibling_indent = item_indent + 2
                else:
                    elem = {key.strip(): None}
                    sub_val, j, child_indent = _parse_indented(lines, i + 1)
                    elem[key.strip()] = sub_val
                    sibling_indent = item_indent + 2
                    # `child_indent` tells us this item's inner
                    # block fully consumed; now look for more keys
                    # at `sibling_indent`.
                    _ = child_indent
                while j < len(lines):
                    deeper_ind, deeper = lines[j]
                    if deeper_ind < sibling_indent or deeper.startswith("- "):
                        break
                    if deeper_ind > sibling_indent:
                        break
                    dkey, _, drest = deeper.partition(":")
                    drest = drest.strip()
                    if drest:
                        elem[dkey.strip()] = _scalar(drest)
                        j += 1
                    else:
                        sub_val, j, _ = _parse_indented(lines, j + 1)
                        elem[dkey.strip()] = sub_val
                out.append(elem)
                i = j
            else:
                out.append(_scalar(content))
                i += 1
        return out, i
    # Mapping block.
    block_indent = ind if ind >= indent else indent
    obj: dict = {}
    i = start
    while i < len(lines):
        cur_ind, cur = lines[i]
        if cur_ind < block_indent:
            break
        if cur_ind > block_indent:
            # Stray over-indent — shouldn't happen for well-formed
            # YAML; skip to avoid an infinite loop.
            i += 1
            continue
        key, _, rest = cur.partition(":")
        rest = rest.strip()
        if rest:
            obj[key.strip()] = _scalar(rest)
            i += 1
        else:
            sub_val, i, _ = _parse_indented(lines, i + 1)
            obj[key.strip()] = sub_val
    return obj, i


def _parse_indented(lines: list[tuple[int, str]], start: int):
    """Parse the sub-block that starts at `start`, anchoring on the
    first non-blank line's indent. Returns (value, next_index,
    child_indent) — `child_indent` is useful to callers that want
    to assert the layout they just parsed."""
    if start >= len(lines):
        return None, start, 0
    child_indent = lines[start][0]
    val, j = _parse_block(lines, start, child_indent)
    return val, j, child_indent


def loads(text: str):
    indented: list[tuple[int, str]] = []
    for raw in text.splitlines():
        stripped = _strip_comment(raw)
        if not stripped.strip():
            continue
        ind = len(stripped) - len(stripped.lstrip(" "))
        indented.append((ind, stripped.strip()))
    if not indented:
        return None
    val, _ = _parse_block(indented, 0, indented[0][0])
    return val


# ---------------------------------------------------------------------
# Rule engine
# ---------------------------------------------------------------------


class Dialplan:
    def __init__(self, rules: list[dict]):
        self.rules: list[tuple[dict, dict]] = []
        for r in rules or []:
            match = r.get("match") or {}
            rewrite = r.get("rewrite") or {}
            self.rules.append((match, rewrite))

    def route(self, req: dict) -> dict:
        from_uri = req.get("from") or ""
        to_uri = req.get("to") or ""
        now = datetime.datetime.now()
        for match, rewrite in self.rules:
            if "from" in match and not re.search(match["from"], from_uri):
                continue
            if "to" in match and not re.search(match["to"], to_uri):
                continue
            if "hour_range" in match:
                hr = match["hour_range"]
                if (isinstance(hr, list) and len(hr) == 2
                        and not (hr[0] <= now.hour < hr[1])):
                    continue
            return {
                "target": rewrite.get("target"),
                "reason": rewrite.get("reason", ""),
            }
        return {}


def load_rules(path: Path) -> Dialplan:
    if not path.exists():
        log(f"rules file `{path}` missing — returning empty dialplan")
        return Dialplan([])
    text = path.read_text(encoding="utf-8")
    data = loads(text)
    if not isinstance(data, dict) or "rules" not in data:
        log(f"rules file `{path}` has no `rules:` top-level key")
        return Dialplan([])
    return Dialplan(data["rules"])


def main() -> int:
    here = Path(__file__).resolve().parent
    rules_path = here / RULES_FILE if not os.path.isabs(RULES_FILE) else Path(RULES_FILE)
    dialplan = load_rules(rules_path)
    log(f"ready (rules={rules_path} count={len(dialplan.rules)})")

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError as e:
            reply(None, error={"code": -32700, "message": f"parse error: {e}"})
            continue

        id_ = req.get("id")
        method = req.get("method")
        params = req.get("params") or {}

        if method == "describe_capabilities":
            reply(id_, result=[DESCRIPTOR])
        elif method == "route":
            try:
                reply(id_, result=dialplan.route(params))
            except Exception as e:  # noqa: BLE001
                reply(id_, error={"code": -32603, "message": f"route failed: {e}"})
        elif method == "reload_rules":
            # Dedicated reload hook — distinct from the engine's
            # plugin-level `reload_plugin` (which respawns the
            # whole process). This keeps the process up + reloads
            # just the rules file. Useful when you only edit
            # rules.yaml.
            dialplan = load_rules(rules_path)
            reply(id_, result={"count": len(dialplan.rules)})
        elif method == "shutdown":
            reply(id_, result=None)
            log("shutdown — exiting")
            return 0
        elif method == "ping":
            reply(id_, result={"ok": True})
        else:
            reply(id_, error={"code": -32601, "message": f"method `{method}` not implemented"})
    return 0


if __name__ == "__main__":
    sys.exit(main())
