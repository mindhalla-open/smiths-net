# Browser WebTransport demo

Slice 5.7 / P19 scaffold. Executable documentation of the
`WtSignal` wire format defined in
`crates/smiths-sip/src/webtransport.rs`.

## Status: scaffold only

The engine's QUIC runtime isn't wired yet — the `NullWebTransportListener`
refuses `bind` with a clear `ScaffoldOnly` error. This page is a
faithful rendering of the **protocol shape** a future runtime will
accept, so browser authors can build against it today and light up
real media when the runtime lands.

## Running

Open `index.html` in any browser with WebTransport support (Chrome
or Edge today; Firefox in beta). The buttons emit real `WtSignal`
frames — the `Connect` button fails with the engine's `ScaffoldOnly`
error until the runtime lands.

You can also test the encoding round-trip by pointing a local
`nc -u -l 7880` at the page and watching the hex bytes line up with
the `WtSignal::decode` tests in the Rust crate.

## Frame reference

| Frame            | Direction       | Example                                                                 |
|------------------|-----------------|-------------------------------------------------------------------------|
| `session-init`   | client → engine | `{"type":"session-init","tag":"demo-browser"}`                          |
| `session-ack`    | engine → client | `{"type":"session-ack","session_id":17,"protocol_version":1}`           |
| `offer`          | client → engine | `{"type":"offer","session_id":17,"sdp":"v=0\r\n..."}`                   |
| `answer`         | engine → client | `{"type":"answer","session_id":17,"sdp":"v=0\r\n..."}`                  |
| `ice-candidate`  | both            | `{"type":"ice-candidate","session_id":17,"candidate":"candidate:...","sdp_m_line_index":0}` |
| `ice-end`        | both            | `{"type":"ice-end","session_id":17}`                                    |
| `bye`            | both            | `{"type":"bye","session_id":17,"reason":"user hung up"}`                |
| `error`          | engine → client | `{"type":"error","session_id":17,"code":"codec-mismatch","reason":"..."}`|
| `echo`           | both            | `{"type":"echo","session_id":17,"payload_b64":"aGVsbG8="}` (echoes "hello") |

See `docs/deployment/webtransport.md` for the ops-side story.
