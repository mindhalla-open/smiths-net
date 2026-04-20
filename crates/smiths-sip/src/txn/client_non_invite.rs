//! Client non-INVITE transaction FSM (RFC 3261 §17.1.2).
//!
//! Smallest of the four transaction FSMs. Used by every non-INVITE
//! method the engine originates — OPTIONS, BYE, REGISTER, CANCEL[^1]
//! — so it's the most productive first slice when migrating off the
//! ad-hoc UAC retransmit path.
//!
//! [^1]: CANCEL shares the branch of its target INVITE but runs its
//! own non-INVITE transaction — RFC 3261 §9.1.
//!
//! ## State diagram (unreliable / UDP transport)
//!
//! ```text
//!                            +-----------+
//!             |Request sent  |   Trying  |----------------+
//!             v              +-----------+                |
//!                            | 1xx       | 200-699        |
//!                            v           v                |
//!                     +------------+ +------------+       |
//!                     | Proceeding | | Completed  |       |
//!                     +------------+ +------------+       |
//!                        |  1xx  |  |  200-699      |     |
//!                        |  \\---/  |  \\---/         |     |
//!                        | 200-699 |                 |     |
//!                        +---->----+   Timer K fires |     |
//!                                      v             |     |
//!                                  +------------+    |     |
//!                                  | Terminated |<---+-----+
//!                                  +------------+
//!                                      ^
//!                                      | Timer F fires (timeout)
//! ```
//!
//! ## Timers (RFC 3261 §17.1.2.2)
//!
//! - **E** (retransmit): `T1 = 500 ms`, doubles each retry up to
//!   `T2 = 4 s`. Cancelled on entering Completed/Terminated.
//! - **F** (timeout): `64 · T1 = 32 s`. Fires → transaction
//!   Terminates (the TU sees a synthetic timeout response the
//!   driver synthesizes; this FSM just tears down cleanly).
//! - **K** (wait for dup responses): `T4 = 5 s`. Armed on entering
//!   Completed; absorbs late duplicate final responses so the driver
//!   doesn't surface them to the TU twice.
//!
//! Reliable transport (TCP/TLS/SCTP) turns timer E into a no-op and
//! timer K into zero — we don't implement that branch yet because
//! the current driver story is UDP-first; add an
//! `ClientNonInviteTxn::new_reliable` constructor when the TCP
//! driver lands.

use bytes::Bytes;

use super::{
    Role, T1, T2, T4, TIMEOUT_64T1, TimerId, Transaction, TransactionAction, TransactionEvent,
    TransactionKey, TransactionState, timers::doubling_backoff,
};

/// Client non-INVITE transaction FSM.
///
/// Construct with [`Self::new`] (provides the initial request
/// bytes). Feed [`TransactionEvent`]s via [`Transaction::on_event`];
/// the returned [`TransactionAction`]s tell the driver what to do.
///
/// The FSM owns no I/O. All bytes flow in/out via events + actions.
pub struct ClientNonInviteTxn {
    key: TransactionKey,
    state: TransactionState,
    /// Request bytes — we retain them so timer-E retransmits can
    /// resend verbatim without re-serialising at the TU layer.
    request: Bytes,
    /// Attempt counter for timer E's exponential backoff. Starts at
    /// 0 after the first send; `doubling_backoff(attempt, T2)` gives
    /// the next interval.
    attempt: u32,
}

impl ClientNonInviteTxn {
    /// Build a transaction for the outbound request `bytes` that
    /// belongs to `branch` + `method`. The FSM starts in
    /// [`TransactionState::Trying`]; call [`Transaction::on_event`]
    /// with [`TransactionEvent::StartClient`] to trigger the first
    /// send + timer arms.
    pub fn new(branch: impl Into<String>, method: impl Into<String>, request: Bytes) -> Self {
        Self {
            key: TransactionKey {
                branch: branch.into(),
                method: method.into(),
                role: Role::Client,
            },
            state: TransactionState::Trying,
            request,
            attempt: 0,
        }
    }

    /// Transition helper: move to Completed, arm timer K (wait for
    /// duplicate responses), cancel the retransmit path. Actions
    /// common to both Trying→Completed and Proceeding→Completed.
    fn enter_completed(&mut self, final_bytes: Bytes, status: u16) -> Vec<TransactionAction> {
        self.state = TransactionState::Completed;
        vec![
            // Deliver the final response to the TU first — dialog
            // layer updates its state before we retire the timers.
            TransactionAction::DeliverResponseToTu {
                status,
                bytes: final_bytes,
            },
            // Retransmit / timeout timers die here.
            TransactionAction::CancelTimer(TimerId::E),
            TransactionAction::CancelTimer(TimerId::F),
            // K absorbs duplicate final responses.
            TransactionAction::ArmTimer {
                id: TimerId::K,
                after: T4,
            },
        ]
    }
}

impl Transaction for ClientNonInviteTxn {
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
            // --- Initial send from Trying ------------------------------
            (S::Trying, Ev::StartClient) => {
                // Timer E armed at T1, F at 64·T1. E doubles per
                // subsequent fire (up to T2); we don't pre-compute
                // all retries here — driver re-invokes us via the
                // timer-fired event and we arm the next interval.
                vec![
                    TransactionAction::SendToPeer(self.request.clone()),
                    TransactionAction::ArmTimer {
                        id: TimerId::E,
                        after: T1,
                    },
                    TransactionAction::ArmTimer {
                        id: TimerId::F,
                        after: TIMEOUT_64T1,
                    },
                ]
            }

            // --- Timer E in Trying — retransmit ------------------------
            (S::Trying, Ev::TimerFired(TimerId::E)) => {
                self.attempt = self.attempt.saturating_add(1);
                // Per §17.1.2.2 timer E doubles up to T2 while still
                // in Trying; once in Proceeding the cap is T2 from
                // the start. Same doubling helper covers both paths
                // — Proceeding's branch below passes a lower cap.
                let next = doubling_backoff(self.attempt, T2);
                vec![
                    TransactionAction::SendToPeer(self.request.clone()),
                    TransactionAction::ArmTimer {
                        id: TimerId::E,
                        after: next,
                    },
                ]
            }

            // --- Provisional while still in Trying → Proceeding --------
            (S::Trying, Ev::ResponseReceived { status, bytes }) if (100..200).contains(&status) => {
                self.state = S::Proceeding;
                // Proceeding uses a fixed T2 retransmit cadence
                // (§17.1.2.2) — re-arm at that cadence immediately.
                vec![
                    TransactionAction::DeliverResponseToTu { status, bytes },
                    TransactionAction::ArmTimer {
                        id: TimerId::E,
                        after: T2,
                    },
                ]
            }

            // --- Provisional while in Proceeding — deliver + keep timer
            (S::Proceeding, Ev::ResponseReceived { status, bytes })
                if (100..200).contains(&status) =>
            {
                // Timer E already scheduled at T2; nothing to re-arm.
                vec![TransactionAction::DeliverResponseToTu { status, bytes }]
            }

            // --- Timer E in Proceeding — retransmit at T2 --------------
            (S::Proceeding, Ev::TimerFired(TimerId::E)) => {
                vec![
                    TransactionAction::SendToPeer(self.request.clone()),
                    TransactionAction::ArmTimer {
                        id: TimerId::E,
                        after: T2,
                    },
                ]
            }

            // --- Final (2xx-6xx) from Trying or Proceeding → Completed -
            (S::Trying | S::Proceeding, Ev::ResponseReceived { status, bytes })
                if (200..700).contains(&status) =>
            {
                self.enter_completed(bytes, status)
            }

            // --- Timer F — transaction timeout -------------------------
            (S::Trying | S::Proceeding, Ev::TimerFired(TimerId::F)) => {
                self.state = S::Terminated;
                vec![
                    TransactionAction::CancelTimer(TimerId::E),
                    TransactionAction::Terminated,
                ]
            }

            // --- Late duplicate response in Completed — swallow --------
            (S::Completed, Ev::ResponseReceived { .. }) => {
                // RFC 3261 §17.1.2.2: any response in Completed is
                // silently consumed. Timer K stays armed; driver will
                // drop us when it fires.
                Vec::new()
            }

            // --- Timer K in Completed — done ---------------------------
            (S::Completed, Ev::TimerFired(TimerId::K)) => {
                self.state = S::Terminated;
                vec![TransactionAction::Terminated]
            }

            // --- Anything else — ignore silently -----------------------
            //
            // The FSM is authoritative about what's valid; illegal
            // combinations (e.g. a timer E firing in Terminated
            // because the driver was slow to cancel it) are absorbed
            // rather than panicked — real SIP stacks see these race
            // conditions all the time.
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn req_bytes() -> Bytes {
        Bytes::from_static(b"BYE sip:alice@a.invalid SIP/2.0\r\n\r\n")
    }

    fn new_txn() -> ClientNonInviteTxn {
        ClientNonInviteTxn::new("z9hG4bK-test", "BYE", req_bytes())
    }

    /// Strip out just the action variants we care about for assertions
    /// without building a whole-vec `matches!` tree.
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

    #[test]
    fn start_client_sends_request_and_arms_e_and_f() {
        let mut t = new_txn();
        assert_eq!(t.state(), TransactionState::Trying);
        let actions = t.on_event(TransactionEvent::StartClient);
        assert!(has_send(&actions));
        assert!(has_timer(&actions, TimerId::E));
        assert!(has_timer(&actions, TimerId::F));
        // Assert the E interval is T1 on the first arm.
        let e_after = actions.iter().find_map(|a| {
            if let TransactionAction::ArmTimer {
                id: TimerId::E,
                after,
            } = a
            {
                Some(*after)
            } else {
                None
            }
        });
        assert_eq!(e_after, Some(T1));
    }

    #[test]
    fn timer_e_in_trying_retransmits_and_doubles() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::StartClient);
        let a1 = t.on_event(TransactionEvent::TimerFired(TimerId::E));
        assert!(has_send(&a1));
        let first_retry = a1
            .iter()
            .find_map(|a| {
                if let TransactionAction::ArmTimer {
                    id: TimerId::E,
                    after,
                } = a
                {
                    Some(*after)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(
            first_retry,
            Duration::from_secs(1),
            "attempt 1 = 2·T1 = 1 s"
        );

        // Second retry doubles again to 2 s.
        let a2 = t.on_event(TransactionEvent::TimerFired(TimerId::E));
        let second_retry = a2
            .iter()
            .find_map(|a| {
                if let TransactionAction::ArmTimer {
                    id: TimerId::E,
                    after,
                } = a
                {
                    Some(*after)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(second_retry, Duration::from_secs(2));
    }

    #[test]
    fn provisional_in_trying_transitions_to_proceeding() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::StartClient);
        let actions = t.on_event(TransactionEvent::ResponseReceived {
            status: 100,
            bytes: Bytes::from_static(b"SIP/2.0 100 Trying\r\n\r\n"),
        });
        assert_eq!(t.state(), TransactionState::Proceeding);
        assert_eq!(delivered_status(&actions), Some(100));
        // Re-arm E at T2 per §17.1.2.2.
        let after = actions.iter().find_map(|a| {
            if let TransactionAction::ArmTimer {
                id: TimerId::E,
                after,
            } = a
            {
                Some(*after)
            } else {
                None
            }
        });
        assert_eq!(after, Some(T2));
    }

    #[test]
    fn provisional_in_proceeding_delivers_only() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::StartClient);
        // Move to Proceeding.
        t.on_event(TransactionEvent::ResponseReceived {
            status: 100,
            bytes: Bytes::new(),
        });
        let actions = t.on_event(TransactionEvent::ResponseReceived {
            status: 183,
            bytes: Bytes::new(),
        });
        assert_eq!(delivered_status(&actions), Some(183));
        // No timer re-arm on repeat 1xx.
        assert!(!has_timer(&actions, TimerId::E));
    }

    #[test]
    fn final_response_from_trying_enters_completed() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::StartClient);
        let actions = t.on_event(TransactionEvent::ResponseReceived {
            status: 200,
            bytes: Bytes::from_static(b"SIP/2.0 200 OK\r\n\r\n"),
        });
        assert_eq!(t.state(), TransactionState::Completed);
        assert_eq!(delivered_status(&actions), Some(200));
        assert!(has_cancel(&actions, TimerId::E));
        assert!(has_cancel(&actions, TimerId::F));
        assert!(has_timer(&actions, TimerId::K));
    }

    #[test]
    fn final_response_from_proceeding_enters_completed() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::StartClient);
        t.on_event(TransactionEvent::ResponseReceived {
            status: 100,
            bytes: Bytes::new(),
        });
        let actions = t.on_event(TransactionEvent::ResponseReceived {
            status: 404,
            bytes: Bytes::new(),
        });
        assert_eq!(t.state(), TransactionState::Completed);
        assert_eq!(delivered_status(&actions), Some(404));
        assert!(has_timer(&actions, TimerId::K));
    }

    #[test]
    fn duplicate_final_in_completed_is_silently_dropped() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::StartClient);
        t.on_event(TransactionEvent::ResponseReceived {
            status: 200,
            bytes: Bytes::new(),
        });
        assert_eq!(t.state(), TransactionState::Completed);
        let dup = t.on_event(TransactionEvent::ResponseReceived {
            status: 200,
            bytes: Bytes::new(),
        });
        assert!(
            dup.is_empty(),
            "duplicate final must not re-deliver or re-arm"
        );
    }

    #[test]
    fn timer_k_terminates_after_completed() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::StartClient);
        t.on_event(TransactionEvent::ResponseReceived {
            status: 200,
            bytes: Bytes::new(),
        });
        let actions = t.on_event(TransactionEvent::TimerFired(TimerId::K));
        assert_eq!(t.state(), TransactionState::Terminated);
        assert!(has_terminated(&actions));
    }

    #[test]
    fn timer_f_times_out_trying() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::StartClient);
        let actions = t.on_event(TransactionEvent::TimerFired(TimerId::F));
        assert_eq!(t.state(), TransactionState::Terminated);
        assert!(has_cancel(&actions, TimerId::E));
        assert!(has_terminated(&actions));
    }

    #[test]
    fn timer_f_times_out_proceeding() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::StartClient);
        t.on_event(TransactionEvent::ResponseReceived {
            status: 100,
            bytes: Bytes::new(),
        });
        let actions = t.on_event(TransactionEvent::TimerFired(TimerId::F));
        assert_eq!(t.state(), TransactionState::Terminated);
        assert!(has_terminated(&actions));
    }

    #[test]
    fn events_in_terminated_are_ignored() {
        let mut t = new_txn();
        t.on_event(TransactionEvent::StartClient);
        t.on_event(TransactionEvent::TimerFired(TimerId::F));
        assert_eq!(t.state(), TransactionState::Terminated);
        // Stray late response / stale timer fire must be no-op.
        let a1 = t.on_event(TransactionEvent::ResponseReceived {
            status: 200,
            bytes: Bytes::new(),
        });
        let a2 = t.on_event(TransactionEvent::TimerFired(TimerId::E));
        assert!(a1.is_empty() && a2.is_empty());
    }

    #[test]
    fn key_roundtrips() {
        let t = new_txn();
        assert_eq!(t.key().branch, "z9hG4bK-test");
        assert_eq!(t.key().method, "BYE");
        assert_eq!(t.key().role, Role::Client);
    }
}
