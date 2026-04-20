//! Server INVITE transaction FSM (RFC 3261 §17.2.1).
//!
//! Drives the server side of an INVITE from the moment the request
//! hits the transaction layer through to ACK (for non-2xx) or
//! hand-off to the dialog layer (for 2xx). The FSM owns the
//! retransmit buffer for non-2xx finals — this **replaces** the UAS's
//! ad-hoc `dedupe` `DashMap` once the UAS migration lands in a later
//! slice.
//!
//! ## State diagram (unreliable / UDP)
//!
//! ```text
//!                  |INVITE from network
//!                  v
//!            +-------------+
//!            | Proceeding  |  (auto: TU may send 1xx, which just
//!            +-------------+   reflects out — no state change)
//!              |     |
//!  2xx from TU |     | 300-699 from TU
//!              |     v
//!              |  +------------+
//!              |  | Completed  | Timer G retransmits final.
//!              |  +------------+ Timer H waits for ACK.
//!              |      |
//!              |      | ACK received
//!              |      v
//!              |  +------------+
//!              |  | Confirmed  | Timer I absorbs ACK retransmits.
//!              |  +------------+
//!              |      |
//!              |      | Timer I fires
//!              v      v
//!          +------------+
//!          | Terminated |
//!          +------------+
//!              ^
//!              | Timer H fires (ACK never came)
//! ```
//!
//! ## Timers (RFC 3261 §17.2.1)
//!
//! - **G** (retransmit non-2xx final): T1, doubles up to T2. Armed
//!   on entering Completed, cancelled on ACK receipt / on leaving.
//! - **H** (wait for ACK): `64 · T1 = 32 s`. Fires →
//!   Terminated (peer's transport died).
//! - **I** (absorb ACK retransmits): `T4 = 5 s`. Armed on entering
//!   Confirmed; fires → Terminated.
//!
//! ## 2xx bypass
//!
//! Per §17.2.1, 2xx finals bypass the Completed / Confirmed dance:
//! the FSM terminates immediately, and the TU / dialog layer owns
//! the end-to-end ACK retransmission per §13.3.1.4. The transaction
//! layer has nothing useful to add for 2xx.

use bytes::Bytes;

use super::{
    Role, T1, T2, T4, TIMEOUT_64T1, TimerId, Transaction, TransactionAction, TransactionEvent,
    TransactionKey, TransactionState, timers::doubling_backoff,
};

/// Server INVITE transaction FSM.
pub struct ServerInviteTxn {
    key: TransactionKey,
    state: TransactionState,
    /// Last response we sent on this transaction. Used to:
    ///   - re-send on INVITE retransmit (before the final lands —
    ///     §17.2.1 "the TU MAY re-send its provisional response");
    ///   - drive timer G retransmits in Completed.
    last_response: Option<Bytes>,
    /// Attempt counter for timer G's doubling-up-to-T2 schedule.
    g_attempt: u32,
}

impl ServerInviteTxn {
    /// Build a server-side FSM for an incoming INVITE. The caller
    /// (UAS) has already parsed the request; `branch` is the Via
    /// branch that identifies this hop-to-hop exchange.
    ///
    /// FSM starts in [`TransactionState::Proceeding`] per §17.2.1 —
    /// the request has been accepted by the transaction layer and
    /// is waiting for the TU to decide on a response.
    pub fn new(branch: impl Into<String>) -> Self {
        Self {
            key: TransactionKey {
                branch: branch.into(),
                method: "INVITE".to_owned(),
                role: Role::Server,
            },
            state: TransactionState::Proceeding,
            last_response: None,
            g_attempt: 0,
        }
    }
}

impl Transaction for ServerInviteTxn {
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
            // --- TU sends a provisional while in Proceeding --------------
            (S::Proceeding, Ev::SendResponseFromTu { status, bytes })
                if (100..200).contains(&status) =>
            {
                self.last_response = Some(bytes.clone());
                vec![TransactionAction::SendToPeer(bytes)]
            }

            // --- TU sends a 2xx — bypass to Terminated -------------------
            (S::Proceeding, Ev::SendResponseFromTu { status, bytes })
                if (200..300).contains(&status) =>
            {
                self.state = S::Terminated;
                // Send the 2xx one time; dialog layer retransmits
                // it itself with exponentially-doubling timers
                // (RFC 3261 §13.3.1.4) so the transaction layer
                // needs no further bookkeeping.
                vec![
                    TransactionAction::SendToPeer(bytes),
                    TransactionAction::Terminated,
                ]
            }

            // --- TU sends a non-2xx final — enter Completed -------------
            (S::Proceeding, Ev::SendResponseFromTu { status, bytes })
                if (300..700).contains(&status) =>
            {
                self.state = S::Completed;
                self.last_response = Some(bytes.clone());
                vec![
                    TransactionAction::SendToPeer(bytes),
                    TransactionAction::ArmTimer {
                        id: TimerId::G,
                        after: T1,
                    },
                    TransactionAction::ArmTimer {
                        id: TimerId::H,
                        after: TIMEOUT_64T1,
                    },
                ]
            }

            // --- INVITE retransmit in Proceeding — replay last 1xx ------
            (S::Proceeding, Ev::RequestReceived { ref method, .. }) if method == "INVITE" => {
                if let Some(last) = &self.last_response {
                    vec![TransactionAction::SendToPeer(last.clone())]
                } else {
                    // No response yet — drop silently (§17.2.1
                    // "otherwise the request MUST be discarded").
                    Vec::new()
                }
            }

            // --- Timer G — retransmit non-2xx final ---------------------
            (S::Completed, Ev::TimerFired(TimerId::G)) => {
                self.g_attempt = self.g_attempt.saturating_add(1);
                let next = doubling_backoff(self.g_attempt, T2);
                if let Some(last) = &self.last_response {
                    vec![
                        TransactionAction::SendToPeer(last.clone()),
                        TransactionAction::ArmTimer {
                            id: TimerId::G,
                            after: next,
                        },
                    ]
                } else {
                    // Shouldn't happen; defensive.
                    Vec::new()
                }
            }

            // --- INVITE retransmit in Completed — re-send the final ----
            (S::Completed, Ev::RequestReceived { ref method, .. }) if method == "INVITE" => {
                if let Some(last) = &self.last_response {
                    vec![TransactionAction::SendToPeer(last.clone())]
                } else {
                    Vec::new()
                }
            }

            // --- ACK for the non-2xx final — enter Confirmed ------------
            (S::Completed, Ev::RequestReceived { ref method, .. }) if method == "ACK" => {
                self.state = S::Confirmed;
                vec![
                    TransactionAction::CancelTimer(TimerId::G),
                    TransactionAction::CancelTimer(TimerId::H),
                    TransactionAction::ArmTimer {
                        id: TimerId::I,
                        after: T4,
                    },
                ]
            }

            // --- ACK retransmits in Confirmed — swallow silently --------
            (S::Confirmed, Ev::RequestReceived { ref method, .. }) if method == "ACK" => {
                // §17.2.1: "Any retransmissions of the ACK received
                // in this state MUST be silently ignored."
                Vec::new()
            }

            // --- Timer H — ACK never arrived, transport failure ---------
            (S::Completed, Ev::TimerFired(TimerId::H)) => {
                self.state = S::Terminated;
                vec![
                    TransactionAction::CancelTimer(TimerId::G),
                    TransactionAction::Terminated,
                ]
            }

            // --- Timer I — ACK absorption window closed -----------------
            (S::Confirmed, Ev::TimerFired(TimerId::I)) => {
                self.state = S::Terminated;
                vec![TransactionAction::Terminated]
            }

            // Anything else absorbs. Stray requests (non-INVITE /
            // non-ACK), stray timers that race cancellation, TU
            // events in terminal states — all silent no-ops.
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_txn() -> ServerInviteTxn {
        ServerInviteTxn::new("z9hG4bK-siv")
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
    fn count_sends(actions: &[TransactionAction]) -> usize {
        actions
            .iter()
            .filter(|a| matches!(a, TransactionAction::SendToPeer(_)))
            .count()
    }

    #[test]
    fn starts_in_proceeding() {
        let t = new_txn();
        assert_eq!(t.state(), TransactionState::Proceeding);
        assert_eq!(t.key().method, "INVITE");
        assert_eq!(t.key().role, Role::Server);
    }

    #[test]
    fn tu_provisional_sends_and_stays_in_proceeding() {
        let mut t = new_txn();
        let a = t.on_event(TransactionEvent::SendResponseFromTu {
            status: 100,
            bytes: Bytes::from_static(b"SIP/2.0 100 Trying\r\n\r\n"),
        });
        assert!(has_send(&a));
        assert_eq!(t.state(), TransactionState::Proceeding);
    }

    #[test]
    fn tu_2xx_bypasses_to_terminated() {
        let mut t = new_txn();
        let a = t.on_event(TransactionEvent::SendResponseFromTu {
            status: 200,
            bytes: Bytes::from_static(b"SIP/2.0 200 OK\r\n\r\n"),
        });
        assert!(has_send(&a));
        assert!(has_terminated(&a));
        assert_eq!(t.state(), TransactionState::Terminated);
        // No G/H — TU does its own 2xx retransmit.
        assert!(!has_timer(&a, TimerId::G));
        assert!(!has_timer(&a, TimerId::H));
    }

    #[test]
    fn tu_non_2xx_enters_completed_and_arms_g_and_h() {
        let mut t = new_txn();
        let a = t.on_event(TransactionEvent::SendResponseFromTu {
            status: 486,
            bytes: Bytes::from_static(b"SIP/2.0 486 Busy Here\r\n\r\n"),
        });
        assert_eq!(t.state(), TransactionState::Completed);
        assert!(has_send(&a));
        assert!(has_timer(&a, TimerId::G));
        assert!(has_timer(&a, TimerId::H));
    }

    #[test]
    fn invite_retransmit_replays_last_provisional() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::SendResponseFromTu {
            status: 180,
            bytes: Bytes::from_static(b"SIP/2.0 180 Ringing\r\n\r\n"),
        });
        let a = t.on_event(TransactionEvent::RequestReceived {
            method: "INVITE".into(),
            bytes: Bytes::new(),
        });
        assert_eq!(count_sends(&a), 1);
        assert_eq!(t.state(), TransactionState::Proceeding);
    }

    #[test]
    fn invite_retransmit_before_any_response_is_dropped() {
        let mut t = new_txn();
        let a = t.on_event(TransactionEvent::RequestReceived {
            method: "INVITE".into(),
            bytes: Bytes::new(),
        });
        assert!(a.is_empty(), "no last response to replay = no action");
    }

    #[test]
    fn timer_g_retransmits_with_doubling() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::SendResponseFromTu {
            status: 500,
            bytes: Bytes::from_static(b"SIP/2.0 500 Server Error\r\n\r\n"),
        });
        let first = t.on_event(TransactionEvent::TimerFired(TimerId::G));
        assert!(has_send(&first));
        let first_after = first
            .iter()
            .find_map(|a| {
                if let TransactionAction::ArmTimer {
                    id: TimerId::G,
                    after,
                } = a
                {
                    Some(*after)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(first_after, std::time::Duration::from_secs(1)); // 2·T1
        let second = t.on_event(TransactionEvent::TimerFired(TimerId::G));
        let second_after = second
            .iter()
            .find_map(|a| {
                if let TransactionAction::ArmTimer {
                    id: TimerId::G,
                    after,
                } = a
                {
                    Some(*after)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(second_after, std::time::Duration::from_secs(2)); // 4·T1
    }

    #[test]
    fn invite_retransmit_in_completed_resends_final() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::SendResponseFromTu {
            status: 404,
            bytes: Bytes::from_static(b"SIP/2.0 404 Not Found\r\n\r\n"),
        });
        let a = t.on_event(TransactionEvent::RequestReceived {
            method: "INVITE".into(),
            bytes: Bytes::new(),
        });
        assert_eq!(count_sends(&a), 1, "must replay cached final");
        assert_eq!(t.state(), TransactionState::Completed);
    }

    #[test]
    fn ack_moves_completed_to_confirmed_and_cancels_g_h() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::SendResponseFromTu {
            status: 486,
            bytes: Bytes::from_static(b"SIP/2.0 486 Busy\r\n\r\n"),
        });
        let a = t.on_event(TransactionEvent::RequestReceived {
            method: "ACK".into(),
            bytes: Bytes::new(),
        });
        assert_eq!(t.state(), TransactionState::Confirmed);
        assert!(has_cancel(&a, TimerId::G));
        assert!(has_cancel(&a, TimerId::H));
        assert!(has_timer(&a, TimerId::I));
    }

    #[test]
    fn ack_retransmit_in_confirmed_is_silently_dropped() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::SendResponseFromTu {
            status: 500,
            bytes: Bytes::from_static(b"SIP/2.0 500 Server Error\r\n\r\n"),
        });
        t.on_event(TransactionEvent::RequestReceived {
            method: "ACK".into(),
            bytes: Bytes::new(),
        });
        let dup = t.on_event(TransactionEvent::RequestReceived {
            method: "ACK".into(),
            bytes: Bytes::new(),
        });
        assert!(dup.is_empty());
        assert_eq!(t.state(), TransactionState::Confirmed);
    }

    #[test]
    fn timer_h_times_out_completed() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::SendResponseFromTu {
            status: 500,
            bytes: Bytes::new(),
        });
        let a = t.on_event(TransactionEvent::TimerFired(TimerId::H));
        assert_eq!(t.state(), TransactionState::Terminated);
        assert!(has_cancel(&a, TimerId::G));
        assert!(has_terminated(&a));
    }

    #[test]
    fn timer_i_terminates_confirmed() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::SendResponseFromTu {
            status: 500,
            bytes: Bytes::new(),
        });
        t.on_event(TransactionEvent::RequestReceived {
            method: "ACK".into(),
            bytes: Bytes::new(),
        });
        let a = t.on_event(TransactionEvent::TimerFired(TimerId::I));
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
        assert_eq!(t.state(), TransactionState::Terminated);
        let a1 = t.on_event(TransactionEvent::TimerFired(TimerId::G));
        let a2 = t.on_event(TransactionEvent::RequestReceived {
            method: "ACK".into(),
            bytes: Bytes::new(),
        });
        assert!(a1.is_empty() && a2.is_empty());
    }
}
