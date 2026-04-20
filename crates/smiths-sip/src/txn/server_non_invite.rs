//! Server non-INVITE transaction FSM (RFC 3261 §17.2.2).
//!
//! The simplest of the four transaction FSMs — one timer (J), no ACK
//! handshake, no retransmit schedule for the response. The server
//! just emits whatever the TU hands it, and then absorbs request
//! retransmissions for 64·T1 before retiring.
//!
//! Used by every inbound non-INVITE method the UAS handles: OPTIONS,
//! BYE, REGISTER, MESSAGE, CANCEL, …. Today the UAS still uses its
//! `dedupe` `DashMap` for replay protection; this FSM is the
//! architectural replacement — callable from a future UAS migration
//! slice.
//!
//! ## State diagram (unreliable / UDP)
//!
//! ```text
//!                |Request from network
//!                v
//!          +-----------+
//!          |  Trying   |
//!          +-----------+
//!               |
//!            TU sends 1xx
//!               v
//!          +------------+
//!          | Proceeding |<-- request retransmit (replay last response)
//!          +------------+
//!               |
//!            TU sends 2xx-6xx
//!               v
//!          +------------+
//!          | Completed  |<-- request retransmit (replay final)
//!          +------------+
//!               |
//!            Timer J fires
//!               v
//!          +------------+
//!          | Terminated |
//!          +------------+
//! ```
//!
//! ## Timers (RFC 3261 §17.2.2)
//!
//! - **J** (absorb retransmitted requests): `64 · T1 = 32 s` on
//!   unreliable transport; `0` on reliable. Armed on entering
//!   Completed; fires → Terminated. While armed, any retransmit
//!   of the original request causes the FSM to re-send the cached
//!   final response.

use bytes::Bytes;

use super::{
    Role, TIMEOUT_64T1, TimerId, Transaction, TransactionAction, TransactionEvent, TransactionKey,
    TransactionState,
};

/// Server non-INVITE transaction FSM.
pub struct ServerNonInviteTxn {
    key: TransactionKey,
    state: TransactionState,
    /// Last response emitted. Replayed on request retransmits in
    /// Proceeding / Completed.
    last_response: Option<Bytes>,
}

impl ServerNonInviteTxn {
    /// Build a server-side FSM for an incoming non-INVITE request.
    /// `method` is the request method (e.g. `"BYE"`); `branch` is
    /// the Via branch for this hop-to-hop transaction.
    ///
    /// Starts in [`TransactionState::Trying`] per §17.2.2.
    pub fn new(branch: impl Into<String>, method: impl Into<String>) -> Self {
        Self {
            key: TransactionKey {
                branch: branch.into(),
                method: method.into(),
                role: Role::Server,
            },
            state: TransactionState::Trying,
            last_response: None,
        }
    }
}

impl Transaction for ServerNonInviteTxn {
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
            // --- TU 1xx in Trying → Proceeding ---------------------------
            (S::Trying, Ev::SendResponseFromTu { status, bytes })
                if (100..200).contains(&status) =>
            {
                self.state = S::Proceeding;
                self.last_response = Some(bytes.clone());
                vec![TransactionAction::SendToPeer(bytes)]
            }

            // --- TU final from Trying → Completed ------------------------
            (S::Trying, Ev::SendResponseFromTu { status, bytes })
                if (200..700).contains(&status) =>
            {
                self.state = S::Completed;
                self.last_response = Some(bytes.clone());
                vec![
                    TransactionAction::SendToPeer(bytes),
                    TransactionAction::ArmTimer {
                        id: TimerId::J,
                        after: TIMEOUT_64T1,
                    },
                ]
            }

            // --- TU 1xx in Proceeding — emit, stay -----------------------
            (S::Proceeding, Ev::SendResponseFromTu { status, bytes })
                if (100..200).contains(&status) =>
            {
                self.last_response = Some(bytes.clone());
                vec![TransactionAction::SendToPeer(bytes)]
            }

            // --- TU final from Proceeding → Completed --------------------
            (S::Proceeding, Ev::SendResponseFromTu { status, bytes })
                if (200..700).contains(&status) =>
            {
                self.state = S::Completed;
                self.last_response = Some(bytes.clone());
                vec![
                    TransactionAction::SendToPeer(bytes),
                    TransactionAction::ArmTimer {
                        id: TimerId::J,
                        after: TIMEOUT_64T1,
                    },
                ]
            }

            // --- Request retransmit in Proceeding or Completed -----------
            //
            // §17.2.2: "If a retransmission of the request is received
            // while in the 'Proceeding' state, the most recently sent
            // provisional response MUST be passed to the transport layer
            // for retransmission." Same rule applies in Completed for
            // the cached final.
            (S::Proceeding | S::Completed, Ev::RequestReceived { .. }) => {
                if let Some(last) = &self.last_response {
                    vec![TransactionAction::SendToPeer(last.clone())]
                } else {
                    Vec::new()
                }
            }

            // --- Timer J — retransmit window closed ---------------------
            (S::Completed, Ev::TimerFired(TimerId::J)) => {
                self.state = S::Terminated;
                vec![TransactionAction::Terminated]
            }

            // Everything else absorbs silently: request retransmits in
            // Trying (nothing to replay yet), stray timers racing
            // cancellation, duplicate TU responses, …
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_txn() -> ServerNonInviteTxn {
        ServerNonInviteTxn::new("z9hG4bK-sni", "OPTIONS")
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
    fn has_terminated(actions: &[TransactionAction]) -> bool {
        actions
            .iter()
            .any(|a| matches!(a, TransactionAction::Terminated))
    }
    fn count_sends(actions: &[TransactionAction]) -> usize {
        actions
            .iter()
            .filter(|a| matches!(a, TransactionAction::SendToPeer(_)))
            .count()
    }

    #[test]
    fn starts_in_trying_with_server_role() {
        let t = new_txn();
        assert_eq!(t.state(), TransactionState::Trying);
        assert_eq!(t.key().method, "OPTIONS");
        assert_eq!(t.key().role, Role::Server);
    }

    #[test]
    fn tu_1xx_moves_trying_to_proceeding() {
        let mut t = new_txn();
        let a = t.on_event(TransactionEvent::SendResponseFromTu {
            status: 100,
            bytes: Bytes::from_static(b"SIP/2.0 100 Trying\r\n\r\n"),
        });
        assert!(has_send(&a));
        assert_eq!(t.state(), TransactionState::Proceeding);
    }

    #[test]
    fn tu_final_from_trying_enters_completed_and_arms_j() {
        let mut t = new_txn();
        let a = t.on_event(TransactionEvent::SendResponseFromTu {
            status: 200,
            bytes: Bytes::from_static(b"SIP/2.0 200 OK\r\n\r\n"),
        });
        assert_eq!(t.state(), TransactionState::Completed);
        assert!(has_send(&a));
        assert!(has_timer(&a, TimerId::J));
    }

    #[test]
    fn tu_final_from_proceeding_enters_completed() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::SendResponseFromTu {
            status: 100,
            bytes: Bytes::from_static(b"SIP/2.0 100 Trying\r\n\r\n"),
        });
        let a = t.on_event(TransactionEvent::SendResponseFromTu {
            status: 404,
            bytes: Bytes::from_static(b"SIP/2.0 404 Not Found\r\n\r\n"),
        });
        assert_eq!(t.state(), TransactionState::Completed);
        assert!(has_send(&a));
        assert!(has_timer(&a, TimerId::J));
    }

    #[test]
    fn request_retransmit_in_trying_is_dropped() {
        let mut t = new_txn();
        let a = t.on_event(TransactionEvent::RequestReceived {
            method: "OPTIONS".into(),
            bytes: Bytes::new(),
        });
        assert!(a.is_empty());
    }

    #[test]
    fn request_retransmit_in_proceeding_replays_last_provisional() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::SendResponseFromTu {
            status: 180,
            bytes: Bytes::from_static(b"SIP/2.0 180 Ringing\r\n\r\n"),
        });
        let a = t.on_event(TransactionEvent::RequestReceived {
            method: "OPTIONS".into(),
            bytes: Bytes::new(),
        });
        assert_eq!(count_sends(&a), 1);
    }

    #[test]
    fn request_retransmit_in_completed_replays_final() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::SendResponseFromTu {
            status: 200,
            bytes: Bytes::from_static(b"SIP/2.0 200 OK\r\n\r\n"),
        });
        let a = t.on_event(TransactionEvent::RequestReceived {
            method: "OPTIONS".into(),
            bytes: Bytes::new(),
        });
        assert_eq!(count_sends(&a), 1, "final must be re-sent on retransmit");
    }

    #[test]
    fn timer_j_terminates_completed() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::SendResponseFromTu {
            status: 200,
            bytes: Bytes::new(),
        });
        let a = t.on_event(TransactionEvent::TimerFired(TimerId::J));
        assert_eq!(t.state(), TransactionState::Terminated);
        assert!(has_terminated(&a));
    }

    #[test]
    fn stray_events_in_terminated_are_no_ops() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::SendResponseFromTu {
            status: 200,
            bytes: Bytes::new(),
        });
        t.on_event(TransactionEvent::TimerFired(TimerId::J));
        let a = t.on_event(TransactionEvent::RequestReceived {
            method: "OPTIONS".into(),
            bytes: Bytes::new(),
        });
        assert!(a.is_empty());
    }

    #[test]
    fn second_tu_final_in_completed_is_absorbed() {
        // TU buggily sending a second final — must not leak a second
        // SendToPeer or re-arm J.
        let mut t = new_txn();
        t.on_event(TransactionEvent::SendResponseFromTu {
            status: 200,
            bytes: Bytes::new(),
        });
        let a = t.on_event(TransactionEvent::SendResponseFromTu {
            status: 200,
            bytes: Bytes::new(),
        });
        assert!(a.is_empty());
    }
}
