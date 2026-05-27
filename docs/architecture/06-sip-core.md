# SIP core — transaction & dialog layer notes

Design notes that belong with the `smiths-sip` crate but outgrew the
per-module doc comments. See `01-crate-layout.md` for where
`smiths-sip` fits in the workspace and `00-overview.md` for the
engine's high-level shape.

---

## INVITE 2xx retransmission (RFC 3261 §13.3.1.4)

The INVITE server transaction (`ServerInviteTxn`, §17.2.1) bypasses
straight to **Terminated** on 2xx per RFC — the transaction layer does
not retain the bytes, and does not own retransmit timers. §13.3.1.4
hands that responsibility to the **Transaction User** (here: the UAS),
because 2xx retransmit is dialog-scoped, not transaction-scoped:

- The ACK for a 2xx is a **new end-to-end transaction** with its own
  branch (§17.1.1.3). Binding retransmit to the INVITE's branch would
  miss the cancelling event.
- Dialog state (Early → Confirmed) is what signals "we can stop
  retransmitting," not anything in the transaction FSM.

### The retransmit loop

Lives on the UAS as a per-dialog tokio task, armed when `respond()`
emits a 2xx INVITE. The schedule follows §13.3.1.4 exactly:

```
t = 0       → first 2xx sent (via the FSM's SendToPeer; then FSM → Terminated)
t = T1      → retransmit #1    (T1 = 500 ms)
t = T1+2T1  → retransmit #2    (interval doubles)
...         → double each time, capped at T2 = 4 s
t ≥ 64·T1   → loop exits; dialog would be torn down in a follow-on
```

Each retransmit bumps the cumulative `sip_invite_2xx_retransmits`
counter. Under normal operation the counter barely moves — an ACK
lands before T1 and the first retransmit never fires. A sudden slope
change is the operator's signal that 2xx delivery has started to flake.

### Cancellation paths

The loop exits cleanly on any of:

1. **ACK for 2xx** — `handle_ack()` trips the dialog's
   `CancellationToken`, clears `pending_2xx` off the record, and
   transitions Early → Confirmed. This is the healthy path.
2. **BYE before ACK** — exotic but legal. `handle_bye()` cancels on
   teardown so the loop doesn't keep firing after the dialog is gone.
3. **64·T1 budget exhaustion** — the peer never ACKed. The loop
   emits a warning log and exits; a dedicated BYE follow-on is still
   TODO.
4. **Transport send error** — usually a closed socket during
   shutdown. The loop bows out rather than retrying.

### What lives where

| Field / type                                           | Purpose                                                          |
|--------------------------------------------------------|------------------------------------------------------------------|
| `DialogRecord::pending_2xx: Option<Vec<u8>>`           | Parked 2xx bytes; carried on the serializable record for HA      |
| `UasServer::invite_2xx_retransmits: DashMap<Key, Tok>` | Cancel handles, keyed by dialog — sidetabled so HA stays clean   |
| `spawn_invite_2xx_retransmit(key, bytes, peer)`        | Kicks off the tokio task that walks the schedule                 |
| `cancel_invite_2xx_retransmit(key)`                    | Idempotent cancel hit from `handle_ack` and `handle_bye`         |
| `dialog_for_invite(req)`                               | Matches a retransmitted INVITE back to an open dialog so we drop it |

### Why we drop peer INVITE retransmits

Once a dialog is Early and the TU owns the retransmit cadence,
answering a peer-retransmitted INVITE would inject an off-schedule
2xx and break the T1-doubling contract. The old `invite_2xx_cache`
(LRU DashMap keyed by branch) worked by replaying on each retry; the
per-dialog loop supersedes it entirely. A retransmitted INVITE for a
dialog in `self.dialogs` is silently dropped — the peer will see the
TU's next scheduled retransmit on its own schedule. If no dialog
exists for `(call_id, from-tag)`, the INVITE is genuinely new and
rides the normal path.

---

## Related references

- RFC 3261 §13.3.1.4 — 2xx retransmission by TU.
- RFC 3261 §17.1.1.3 — ACK correlation (non-2xx shares INVITE branch;
  2xx ACK is end-to-end).
- RFC 3261 §17.2.1 — server INVITE FSM (see `smiths-sip/src/txn/server_invite.rs`).
- RFC 3261 §12 — dialog state machine (see `smiths-sip/src/txn/dialog.rs`).
