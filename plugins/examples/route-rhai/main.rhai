// Reference Rhai dialplan — slice 4.1 / P24.
//
// The engine loads this file once via smiths_script::ScriptRuntime,
// invokes `describe_capabilities()` at boot, and calls `route(req)`
// on each inbound INVITE. Hot-reload: save the file and the
// watcher (smiths_plugin::watcher) picks it up; rollback kicks in
// after five consecutive routing errors.
//
// Request shape (subset the engine already exposes):
//   { "from": "sip:alice@example.com",
//     "to":   "sip:support@smiths.local",
//     "call_id": "abc123@smiths.local",
//     "headers": #{ ... } }
//
// Response shape:
//   { "target": "sip:dest@pbx.internal",
//     "reason": "matched support" }
//
// Returning `#{}` tells the UAS to use the default (no rewrite).

fn describe_capabilities() {
    [#{
        "capability": "routing.dialplan",
        "plugin": "route-rhai",
        "model_id": "route-rhai-v1",
        "abi": "1.0",
        "description": "Rhai dialplan: maps `sip:<bucket>@...` to a pool.",
        "priority": 50,
    }]
}

// Tiny lookup table inside `route` so hot-reload picks up edits
// without any module-level state to manage.
fn route(req) {
    let pools = #{
        "support":  "sip:queue-support@pbx.internal",
        "sales":    "sip:queue-sales@pbx.internal",
        "voicebot": "sip:voicebot@agents.internal",
    };

    let to_str = "";
    if req.contains("to") {
        to_str = req["to"];
    }
    if type_of(to_str) != "string" {
        to_str = "";
    }

    // Pull the user part of the `to` URI: `sip:<user>@host`.
    let user = "";
    if to_str.starts_with("sip:") {
        let rest = to_str.sub_string(4);
        let at = rest.index_of("@");
        if at >= 0 {
            user = rest.sub_string(0, at);
        }
    }

    let target = ();
    if pools.contains(user) {
        target = pools[user];
    }
    if target == () {
        return #{}; // fall through to engine default
    }
    #{
        "target": target,
        "reason": "matched pool " + user,
    }
}
