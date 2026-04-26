// Canonical-hook Rhai plugin — full capability + routing example.
//
// Demonstrates the complete Rhai plugin contract:
//   describe_capabilities() → advertise what this plugin can do
//   route(req)              → handle inbound SIP routing requests
//
// The engine invokes `route(req)` on each inbound INVITE.
// Hot-reload: save the file → the watcher picks it up.

fn describe_capabilities() {
    [#{
        "capability": "routing.dialplan",
        "plugin": "canonical-hook-rhai",
        "model_id": "canonical-rhai-v1",
        "abi": "1.0",
        "description": "Rhai dialplan: routes SIP calls by to-URI prefix.",
        "priority": 50,
    }]
}

/// Route inbound calls by matching the `to` URI's user part
/// against a lookup table of known queue prefixes.
fn route(req) {
    let pools = #{
        "support":  "sip:queue-support@pbx.internal",
        "sales":    "sip:queue-sales@pbx.internal",
        "billing":  "sip:queue-billing@pbx.internal",
        "voicebot": "sip:voicebot@agents.internal",
    };

    // Extract the `to` field from the request map.
    let to_str = "";
    if req.contains("to") {
        to_str = req["to"];
    }
    if type_of(to_str) != "string" {
        to_str = "";
    }

    // Pull the user part from `sip:<user>@host`.
    let user = "";
    if to_str.starts_with("sip:") {
        let rest = to_str.sub_string(4);
        let at = rest.index_of("@");
        if at >= 0 {
            user = rest.sub_string(0, at);
        }
    }

    // Look up in pool table.
    let target = ();
    if pools.contains(user) {
        target = pools[user];
    }

    if target == () {
        return #{};  // fall through to engine default
    }

    #{
        "target": target,
        "reason": "matched pool " + user,
    }
}
