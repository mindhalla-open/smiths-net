//! Client INVITE transaction FSM (RFC 3261 §17.1.1).
//!
//! Distinct from the non-INVITE client FSM in three important ways:
//!
//! 1. **Retransmit stops on ANY 1xx** (not only on final). Per
//!    §17.1.1.2, when moving to Proceeding we cancel timer A — the
//!    peer has acknowledged the INVITE with a provisional, no need
//!    to retransmit.
//! 2. **2xx bypasses Completed.** A 2xx final drops the transaction
//!    straight to Terminated; the TU (dialog layer) is responsible
//!    for the end-to-end ACK because 2xx ACK retransmits have
//!    dialog-lifetime semantics (§13.3.1.4) rather than transaction
//!    scope.
//! 3. **Non-2xx ACK is the FSM's job.** Per §17.1.1.3 the transaction
//!    layer builds + sends an ACK for every 3xx-6xx final (and every
//!    retransmit of such). We use [`super::ack::build_non_ok_ack`]
//!    to construct those.
//!
//! ## State diagram (unreliable / UDP)
//!
//! ```text
//!                |INVITE sent by TU
//!                v
//!          +-----------+
//!          |  Calling  |
//!          +-----------+
//!           |        |
//!       1xx |        | 300-699 (build+send ACK)
//!           v        v
//!    +----------+  +------------+
//!    |Proceeding|->| Completed  |
//!    +----------+  +------------+
//!           |         |  timer D fires
//!           |         v
//!        2xx      +------------+
//!           `---->| Terminated |
//!                 +------------+
//!                      ^
//!                      | timer B (transport timeout)
//!                      |
//!                      | 2xx from Proceeding or Calling
//! ```
//!
//! ## Timers (RFC 3261 §17.1.1.2)
//!
//! - **A** (retransmit INVITE): initial T1, doubles each fire. Armed
//!   on Calling, **cancelled the instant we move to Proceeding**.
//! - **B** (transaction timeout): `64 · T1 = 32 s`. Armed for the
//!   entire Calling+Proceeding span; fires → Terminated.
//! - **D** (wait for non-2xx retransmissions): `32 s` on UDP. Armed
//!   on entering Completed; fires → Terminated. During D any
//!   duplicate non-2xx triggers a re-send of the ACK (not a
//!   re-delivery to the TU).

use bytes::Bytes;

use super::{
    Role, T1, TIMEOUT_64T1, TimerId, Transaction, TransactionAction, TransactionEvent,
    TransactionKey, TransactionState, ack::build_non_ok_ack, timers::doubling_backoff,
};

/// Timer D value for unreliable transport: 32 s (RFC 3261 §17.1.1.2).
/// Absorbs non-2xx retransmits long enough that the peer gives up.
const TIMER_D: std::time::Duration = std::time::Duration::from_secs(32);

/// Client INVITE transaction FSM.
pub struct ClientInviteTxn {
    key: TransactionKey,
    state: TransactionState,
    /// Original INVITE bytes, needed for timer-A retransmits and for
    /// ACK construction on non-2xx finals.
    invite: Bytes,
    /// The ACK we built on entering Completed — kept so duplicate
    /// non-2xx finals re-send the exact same bytes (§17.1.1.2
    /// "ACK for the final response MUST be constructed ..." and then
    /// retransmits re-use it).
    ack_for_non_ok: Option<Bytes>,
    /// Timer A retransmit attempt counter (0 = first pending retry).
    attempt: u32,
}

impl ClientInviteTxn {
    /// Build a transaction for the outbound INVITE `bytes`. FSM
    /// starts in [`TransactionState::Calling`]; caller must feed
    /// [`TransactionEvent::StartClient`] to trigger the first send +
    /// timer arms.
    pub fn new(branch: impl Into<String>, request: Bytes) -> Self {
        Self {
            key: TransactionKey {
                branch: branch.into(),
                method: "INVITE".to_owned(),
                role: Role::Client,
            },
            state: TransactionState::Calling,
            invite: request,
            ack_for_non_ok: None,
            attempt: 0,
        }
    }

    /// Enter Completed on a non-2xx final. Build and send the ACK
    /// (stored so retransmits of the same final re-send it), arm D,
    /// cancel A and B, deliver the final to the TU.
    fn enter_completed_non_ok(
        &mut self,
        status: u16,
        final_bytes: Bytes,
    ) -> Vec<TransactionAction> {
        self.state = TransactionState::Completed;
        // Build the ACK once and cache it for timer-D-bracketed
        // retransmits. If we can't parse the INVITE + response enough
        // to build an ACK, we still cancel timers and deliver the
        // final — a badly-formed response is the peer's bug, not
        // ours, and the TU can surface the error.
        let ack_bytes = build_non_ok_ack(&self.invite, &final_bytes);
        self.ack_for_non_ok.clone_from(&ack_bytes);
        let mut actions = vec![
            TransactionAction::DeliverResponseToTu {
                status,
                bytes: final_bytes,
            },
            TransactionAction::CancelTimer(TimerId::A),
            TransactionAction::CancelTimer(TimerId::B),
        ];
        if let Some(ack) = ack_bytes {
            actions.push(TransactionAction::SendToPeer(ack));
        }
        actions.push(TransactionAction::ArmTimer {
            id: TimerId::D,
            after: TIMER_D,
        });
        actions
    }

    /// 2xx bypass: deliver + cancel A/B + Terminate. The TU handles
    /// ACK end-to-end (different semantic than non-2xx).
    fn enter_terminated_on_2xx(
        &mut self,
        status: u16,
        final_bytes: Bytes,
    ) -> Vec<TransactionAction> {
        self.state = TransactionState::Terminated;
        vec![
            TransactionAction::DeliverResponseToTu {
                status,
                bytes: final_bytes,
            },
            TransactionAction::CancelTimer(TimerId::A),
            TransactionAction::CancelTimer(TimerId::B),
            TransactionAction::Terminated,
        ]
    }
}

impl Transaction for ClientInviteTxn {
    fn key(&self) -> &TransactionKey {
        &self.key
    }

    fn state(&self) -> TransactionState {
        self.state
    }

    fn on_event(&mut self, event: TransactionEvent) -> Vec<TransactionAction> {
        use TransactionEvent as Ev;
        use TransactionState as S;

        match (self.state, event) {
            // --- Initial send --------------------------------------------
            (S::Calling, Ev::StartClient) => {
                vec![
                    TransactionAction::SendToPeer(self.invite.clone()),
                    TransactionAction::ArmTimer {
                        id: TimerId::A,
                        after: T1,
                    },
                    TransactionAction::ArmTimer {
                        id: TimerId::B,
                        after: TIMEOUT_64T1,
                    },
                ]
            }

            // --- Timer A — retransmit INVITE, double the interval --------
            (S::Calling, Ev::TimerFired(TimerId::A)) => {
                self.attempt = self.attempt.saturating_add(1);
                // Client INVITE timer A has no cap in the RFC — it
                // keeps doubling until B fires. We pass a huge
                // effective cap (1 h) to the helper; the real stop
                // condition is timer B.
                let cap = std::time::Duration::from_hours(1);
                let next = doubling_backoff(self.attempt, cap);
                vec![
                    TransactionAction::SendToPeer(self.invite.clone()),
                    TransactionAction::ArmTimer {
                        id: TimerId::A,
                        after: next,
                    },
                ]
            }

            // --- 1xx from Calling → Proceeding, cancel A -----------------
            (S::Calling, Ev::ResponseReceived { status, bytes })
                if (100..200).contains(&status) =>
            {
                self.state = S::Proceeding;
                vec![
                    TransactionAction::CancelTimer(TimerId::A),
                    TransactionAction::DeliverResponseToTu { status, bytes },
                ]
            }

            // --- 1xx in Proceeding — deliver only ------------------------
            (S::Proceeding, Ev::ResponseReceived { status, bytes })
                if (100..200).contains(&status) =>
            {
                vec![TransactionAction::DeliverResponseToTu { status, bytes }]
            }

            // --- 2xx from Calling or Proceeding → Terminated (bypass) ----
            (S::Calling | S::Proceeding, Ev::ResponseReceived { status, bytes })
                if (200..300).contains(&status) =>
            {
                self.enter_terminated_on_2xx(status, bytes)
            }

            // --- 3xx-6xx from Calling or Proceeding → Completed + ACK ----
            (S::Calling | S::Proceeding, Ev::ResponseReceived { status, bytes })
                if (300..700).contains(&status) =>
            {
                self.enter_completed_non_ok(status, bytes)
            }

            // --- Timer B — transaction timeout ---------------------------
            (S::Calling | S::Proceeding, Ev::TimerFired(TimerId::B)) => {
                self.state = S::Terminated;
                vec![
                    TransactionAction::CancelTimer(TimerId::A),
                    TransactionAction::Terminated,
                ]
            }

            // --- Duplicate non-2xx in Completed — resend ACK ------------
            (S::Completed, Ev::ResponseReceived { status, .. }) if (300..700).contains(&status) => {
                // §17.1.1.2: retransmits of a final in Completed cause
                // the ACK to be re-passed to the transport layer. No
                // re-delivery to the TU.
                if let Some(ack) = &self.ack_for_non_ok {
                    vec![TransactionAction::SendToPeer(ack.clone())]
                } else {
                    // ACK build failed earlier; nothing we can do.
                    Vec::new()
                }
            }

            // --- Timer D — non-2xx retransmit window closed -------------
            (S::Completed, Ev::TimerFired(TimerId::D)) => {
                self.state = S::Terminated;
                vec![TransactionAction::Terminated]
            }

            // Anything else — absorb silently. Covers stray timers
            // that race cancellation, 2xx arriving while in Completed
            // (shouldn't happen because 2xx skips to Terminated, but
            // defensive), etc.
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INVITE: &[u8] = concat!(
        "INVITE sip:alice@127.0.0.1 SIP/2.0\r\n",
        "Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-inv\r\n",
        "From: <sip:bob@10.0.0.1>;tag=bob\r\n",
        "To: <sip:alice@127.0.0.1>\r\n",
        "Call-ID: cid-civ\r\n",
        "CSeq: 1 INVITE\r\n",
        "Max-Forwards: 70\r\n",
        "Content-Length: 0\r\n\r\n",
    )
    .as_bytes();

    fn rsp(code: u16, with_to_tag: &str) -> Bytes {
        Bytes::from(format!(
            concat!(
                "SIP/2.0 {} Reason\r\n",
                "Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-inv;received=10.0.0.1\r\n",
                "From: <sip:bob@10.0.0.1>;tag=bob\r\n",
                "To: <sip:alice@127.0.0.1>;tag={}\r\n",
                "Call-ID: cid-civ\r\n",
                "CSeq: 1 INVITE\r\n",
                "Content-Length: 0\r\n\r\n",
            ),
            code, with_to_tag,
        ))
    }

    fn new_txn() -> ClientInviteTxn {
        ClientInviteTxn::new("z9hG4bK-inv", Bytes::from_static(INVITE))
    }

    fn has_send(actions: &[TransactionAction]) -> bool {
        actions
            .iter()
            .any(|a| matches!(a, TransactionAction::SendToPeer(_)))
    }
    fn has_timer(actions: &[TransactionAction], id: TimerId) -> bool {
        actions
            .iter()
            .any(|a| matches!(a, TransactionAction::ArmTimer { id: i, .. } if *i == id))
    }
    fn has_cancel(actions: &[TransactionAction], id: TimerId) -> bool {
        actions
            .iter()
            .any(|a| matches!(a, TransactionAction::CancelTimer(i) if *i == id))
    }
    fn has_terminated(actions: &[TransactionAction]) -> bool {
        actions
            .iter()
            .any(|a| matches!(a, TransactionAction::Terminated))
    }
    fn delivered_status(actions: &[TransactionAction]) -> Option<u16> {
        actions.iter().find_map(|a| {
            if let TransactionAction::DeliverResponseToTu { status, .. } = a {
                Some(*status)
            } else {
                None
            }
        })
    }
    /// Count `SendToPeer` actions whose bytes start with the given prefix.
    fn count_sends_with_prefix(actions: &[TransactionAction], prefix: &[u8]) -> usize {
        actions
            .iter()
            .filter(|a| matches!(a, TransactionAction::SendToPeer(b) if b.starts_with(prefix)))
            .count()
    }

    #[test]
    fn start_sends_invite_and_arms_a_b() {
        let mut t = new_txn();
        let actions = t.on_event(TransactionEvent::StartClient);
        assert!(has_send(&actions));
        assert!(has_timer(&actions, TimerId::A));
        assert!(has_timer(&actions, TimerId::B));
        assert_eq!(t.state(), TransactionState::Calling);
    }

    #[test]
    fn timer_a_retransmits_with_doubling() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::StartClient);
        let a1 = t.on_event(TransactionEvent::TimerFired(TimerId::A));
        assert!(has_send(&a1));
        let a1_after = a1
            .iter()
            .find_map(|a| {
                if let TransactionAction::ArmTimer {
                    id: TimerId::A,
                    after,
                } = a
                {
                    Some(*after)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(a1_after, std::time::Duration::from_secs(1)); // 2·T1
        let a2 = t.on_event(TransactionEvent::TimerFired(TimerId::A));
        let a2_after = a2
            .iter()
            .find_map(|a| {
                if let TransactionAction::ArmTimer {
                    id: TimerId::A,
                    after,
                } = a
                {
                    Some(*after)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(a2_after, std::time::Duration::from_secs(2)); // 4·T1
    }

    #[test]
    fn provisional_moves_to_proceeding_and_cancels_a() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::StartClient);
        let a = t.on_event(TransactionEvent::ResponseReceived {
            status: 100,
            bytes: rsp(100, ""),
        });
        assert_eq!(t.state(), TransactionState::Proceeding);
        assert_eq!(delivered_status(&a), Some(100));
        assert!(
            has_cancel(&a, TimerId::A),
            "timer A must die on 1xx (§17.1.1.2)"
        );
        // Timer B still alive — do NOT cancel it here.
        assert!(!has_cancel(&a, TimerId::B));
    }

    #[test]
    fn provisional_in_proceeding_is_delivered_only() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::StartClient);
        t.on_event(TransactionEvent::ResponseReceived {
            status: 100,
            bytes: rsp(100, ""),
        });
        let a = t.on_event(TransactionEvent::ResponseReceived {
            status: 180,
            bytes: rsp(180, ""),
        });
        assert_eq!(delivered_status(&a), Some(180));
        assert!(!has_timer(&a, TimerId::A));
        assert!(!has_cancel(&a, TimerId::A));
    }

    #[test]
    fn two_hundred_ok_terminates_directly() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::StartClient);
        let a = t.on_event(TransactionEvent::ResponseReceived {
            status: 200,
            bytes: rsp(200, "srv"),
        });
        assert_eq!(t.state(), TransactionState::Terminated);
        assert_eq!(delivered_status(&a), Some(200));
        assert!(has_cancel(&a, TimerId::A));
        assert!(has_cancel(&a, TimerId::B));
        assert!(has_terminated(&a));
        // No D armed — 2xx path skips Completed.
        assert!(!has_timer(&a, TimerId::D));
        // No ACK sent — TU does end-to-end ACK for 2xx.
        assert_eq!(count_sends_with_prefix(&a, b"ACK "), 0);
    }

    #[test]
    fn non_2xx_final_sends_ack_and_enters_completed() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::StartClient);
        let a = t.on_event(TransactionEvent::ResponseReceived {
            status: 404,
            bytes: rsp(404, "srv404"),
        });
        assert_eq!(t.state(), TransactionState::Completed);
        assert_eq!(delivered_status(&a), Some(404));
        assert!(has_cancel(&a, TimerId::A));
        assert!(has_cancel(&a, TimerId::B));
        assert!(has_timer(&a, TimerId::D));
        assert_eq!(
            count_sends_with_prefix(&a, b"ACK "),
            1,
            "exactly one ACK sent per §17.1.1.3"
        );
    }

    #[test]
    fn duplicate_non_2xx_in_completed_resends_ack_only() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::StartClient);
        t.on_event(TransactionEvent::ResponseReceived {
            status: 486,
            bytes: rsp(486, "srv486"),
        });
        assert_eq!(t.state(), TransactionState::Completed);
        let dup = t.on_event(TransactionEvent::ResponseReceived {
            status: 486,
            bytes: rsp(486, "srv486"),
        });
        // No re-delivery, no new timer.
        assert!(delivered_status(&dup).is_none());
        assert!(!has_timer(&dup, TimerId::D));
        // ACK is re-sent.
        assert_eq!(count_sends_with_prefix(&dup, b"ACK "), 1);
    }

    #[test]
    fn timer_d_terminates_completed() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::StartClient);
        t.on_event(TransactionEvent::ResponseReceived {
            status: 500,
            bytes: rsp(500, "srv500"),
        });
        let a = t.on_event(TransactionEvent::TimerFired(TimerId::D));
        assert_eq!(t.state(), TransactionState::Terminated);
        assert!(has_terminated(&a));
    }

    #[test]
    fn timer_b_times_out_from_calling() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::StartClient);
        let a = t.on_event(TransactionEvent::TimerFired(TimerId::B));
        assert_eq!(t.state(), TransactionState::Terminated);
        assert!(has_cancel(&a, TimerId::A));
        assert!(has_terminated(&a));
    }

    #[test]
    fn timer_b_times_out_from_proceeding() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::StartClient);
        t.on_event(TransactionEvent::ResponseReceived {
            status: 180,
            bytes: rsp(180, ""),
        });
        let a = t.on_event(TransactionEvent::TimerFired(TimerId::B));
        assert_eq!(t.state(), TransactionState::Terminated);
        assert!(has_terminated(&a));
    }

    #[test]
    fn stray_events_in_terminated_are_no_ops() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::StartClient);
        t.on_event(TransactionEvent::ResponseReceived {
            status: 200,
            bytes: rsp(200, "srv"),
        });
        assert_eq!(t.state(), TransactionState::Terminated);
        let a1 = t.on_event(TransactionEvent::TimerFired(TimerId::A));
        let a2 = t.on_event(TransactionEvent::ResponseReceived {
            status: 180,
            bytes: rsp(180, ""),
        });
        assert!(a1.is_empty() && a2.is_empty());
    }

    #[test]
    fn key_is_invite_client() {
        let t = new_txn();
        assert_eq!(t.key().method, "INVITE");
        assert_eq!(t.key().role, Role::Client);
    }
}
