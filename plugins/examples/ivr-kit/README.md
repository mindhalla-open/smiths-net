# ivr-kit

Reference IVR state machine, authored in Rhai. Pairs DTMF keypresses
(slice 2.4 / P7) with routing decisions the engine's UAS acts on.

## Contract

Every response is a single dict:

| field   | required when              | meaning                                          |
|---------|----------------------------|--------------------------------------------------|
| action  | always                     | `play_prompt`, `transfer`, `hangup`, `record`    |
| prompt  | `action = "play_prompt"`   | prompt file, usually under `prompts/`            |
| target  | `action = "transfer"`      | `sip:` URI the UAS should re-INVITE              |
| state   | usually                    | opaque label threaded back on the next `on_dtmf` |
| done    | always                     | `true` ends the IVR session                      |

## The script

`main.rhai` ships a press-1/press-2/press-3 tree:

```
greeting → main_menu
    1 → transfer sip:queue-sales
    2 → transfer sip:queue-support
    3 → record_message state
    * → replay greeting
    # → hangup
```

Edit + save; the engine's hot-reload watcher (slice 4.1) picks it
up; auto-rollback after five consecutive errors keeps a bad edit
from eating a production call flow.

## Current status (0.44.0)

The Rhai state machine is fully functional and exercised by the
end-to-end test in `crates/smiths-plugin/tests/ivr_state_machine.rs`.
The engine's media-runtime wiring — the glue that lets a live call
send DTMF into `on_dtmf` and the UAS act on a `transfer`/`hangup`
response mid-dialog — is a dedicated follow-on slice; today the
state machine is driven synchronously by tests + tools that build
a synthetic session. The Rhai contract itself is stable.

## Prompt paths

Prompts reference paths the `PromptLibrary` (slice 4.2 Small 1)
knows how to resolve against an operator-configured root. Today
the ivr-kit ships **without** bundled prompt audio — operators
provide their own WAV under `prompts/`; the script only references
them by path.

Use the `record_prompt(call_id, audio_base64, path)` MCP tool
(slice 4.2 Small 2) to capture greetings + write them straight to
the prompt root from an agent.

## Budgets

Manifest caps each `on_dtmf` at 400k ops / 80 ms. A bigger lookup
tree (10-digit phone-tree with per-customer routing) should still
fit comfortably.
