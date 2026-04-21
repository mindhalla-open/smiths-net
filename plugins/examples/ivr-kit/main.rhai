// Reference IVR state machine — slice 4.2 / P9.
//
// The engine drives the IVR by calling three methods on this
// plugin:
//
//   describe_capabilities() -> [descriptor]
//     Standard plugin handshake.
//
//   greet(session) -> { action, prompt?, target?, done }
//     Called once per call when the UAS decides to hand the dialog
//     to the IVR. Returns the first action — typically
//     `"play_prompt"` with the greeting path.
//
//   on_dtmf(session) -> { action, prompt?, target?, done }
//     Called for every DTMF keypress. `session.state` carries the
//     state the last call returned; the engine threads it through
//     verbatim. `session.digit` is the keypress ('0'..'9', '*',
//     '#', 'A'..'D'). The return shape is the same as `greet`.
//
// Response shape (all fields optional):
//   action: "play_prompt" | "transfer" | "hangup" | "record"
//   prompt: "path/to/prompt.wav"        (required when action=play_prompt)
//   target: "sip:dest@host"              (required when action=transfer)
//   state:  "<opaque>"                   (re-issued on next on_dtmf)
//   done:   true                         (end the IVR session)
//
// This reference tree:
//
//   greeting --> main_menu
//       press 1 -> transfer sales pool
//       press 2 -> transfer support pool
//       press 3 -> submenu (record_message)
//       press * -> replay main menu
//       press # -> hangup

fn describe_capabilities() {
    [#{
        "capability": "routing.ivr",
        "plugin": "ivr-kit",
        "model_id": "ivr-kit-v1",
        "abi": "1.0",
        "description": "Press-1-for-sales reference IVR.",
        "priority": 50,
    }]
}

fn greet(session) {
    #{
        "action":  "play_prompt",
        "prompt":  "prompts/welcome.wav",
        "state":   "main_menu",
        "done":    false,
    }
}

fn on_dtmf(session) {
    let state = "main_menu";
    if session.contains("state") {
        state = session["state"];
    }
    let digit = "";
    if session.contains("digit") {
        digit = session["digit"];
    }

    if state == "main_menu" {
        if digit == "1" {
            return #{
                "action": "transfer",
                "target": "sip:queue-sales@pbx.internal",
                "done":   true,
            };
        }
        if digit == "2" {
            return #{
                "action": "transfer",
                "target": "sip:queue-support@pbx.internal",
                "done":   true,
            };
        }
        if digit == "3" {
            return #{
                "action": "play_prompt",
                "prompt": "prompts/leave-message.wav",
                "state":  "record_message",
                "done":   false,
            };
        }
        if digit == "*" {
            return #{
                "action": "play_prompt",
                "prompt": "prompts/welcome.wav",
                "state":  "main_menu",
                "done":   false,
            };
        }
        if digit == "#" {
            return #{ "action": "hangup", "done": true };
        }
        // Unknown digit — replay + stay in state.
        return #{
            "action": "play_prompt",
            "prompt": "prompts/try-again.wav",
            "state":  "main_menu",
            "done":   false,
        };
    }

    if state == "record_message" {
        if digit == "#" {
            return #{ "action": "hangup", "done": true };
        }
        return #{
            "action": "record",
            "state":  "record_message",
            "done":   false,
        };
    }

    // Unknown state — hang up rather than loop forever.
    #{ "action": "hangup", "done": true }
}
