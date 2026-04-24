# Browser WebRTC demo

Static page that talks to the slice 5.10 WebRTC-native
signaling adapter over a plain WebSocket. Sibling of
`examples/browser-webtransport/` — same JSON message shape
(`WtSignal`), different transport.

## Run it

1. Start the engine with `[webrtc] enabled = true` and
   `ws_bind = "127.0.0.1:7881"`:

   ```toml
   [webrtc]
   enabled = true
   ws_bind = "127.0.0.1:7881"
   ```

2. Serve `index.html` from any static server:

   ```sh
   cd examples/browser-webrtc && python3 -m http.server 8080
   ```

3. Open `http://localhost:8080/` in a modern browser.

4. Click **Connect**, then **Send offer**. A real browser offer
   is DTLS-SRTP; today's engine declines that transport and
   replies `offer-rejected: DTLS-SRTP not yet supported`. The
   round-trip proves the signaling path is wired end-to-end.

## What works / what doesn't

| Frame                 | Status                                                    |
|-----------------------|-----------------------------------------------------------|
| `session-init/-ack`   | Works                                                     |
| `offer` (`RTP/AVP`)   | Works — returns a negotiated answer                       |
| `offer` (DTLS-SRTP)   | Rejected with `offer-rejected`; DTLS terminator deferred  |
| `ice-candidate`       | Parsed + ack'd silently                                   |
| `echo`                | Mirrored back                                             |
| `bye`                 | Closes the session                                        |
| Media bridge          | Not wired — no audio carries yet                          |

See `docs/deployment/webrtc.md` for the full state + roadmap.

## TLS / `wss://`

The engine binds plaintext today; front it with nginx or Caddy
for browser `wss://`. The config fields `webrtc.tls_cert` /
`webrtc.tls_key` are accepted but log a warning — they're
reserved for a future in-engine TLS terminator.
