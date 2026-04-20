//! Dialog-layer FSM (RFC 3261 §12).
//!
//! Lives **above** the transaction FSMs. Where a transaction tracks
//! one request/response exchange, a dialog tracks the full call
//! lifetime — from 2xx creation through ACK confirmation to BYE
//! termination. Each dialog hosts many transactions over its life
//! (initial INVITE, re-INVITEs, the final BYE).
//!
//! ## State diagram (RFC 3261 §12.1)
//!
//! ```text
//!         Dialog record created
//!                  |
//!                  v
//!          +-------------+
//!          |    Early    |  (2xx sent/received, ACK not yet seen)
//!          +-------------+
//!             |       |
//!         ACK |       | 3xx-6xx / CANCEL / error
//!             v       v
//!    +------------+  +-------------+
//!    | Confirmed  |->| Terminated  |
//!    +------------+  +-------------+
//!             |           ^
//!          BYE |           |
//!             +-----------+
//! ```
//!
//! The FSM is **pure synchronous** — just a state enum plus
//! `on_event(ev) -> Result`. Call-owning code (today's UAS / UAC)
//! holds a [`DialogFsm`] per dialog and feeds it events as SIP
//! messages flow. Illegal transitions (e.g. `AckReceived` when
//! already Terminated) are explicit `Err(DialogTransitionError)`
//! rather than silent no-ops — that's the distinction from
//! transaction FSMs where stray events absorb. Dialogs are
//! long-lived, so an illegal event usually means an application
//! bug worth surfacing.
//!
//! ## Relationship to `smiths-core::DialogState`
//!
//! `DialogState` in core is the serialization-friendly snapshot type
//! (Early / Confirmed only — kept stable for the HA snapshot path).
//! [`DialogFsmState`] adds `Terminated` and is the in-memory view
//! the FSM enforces transitions on. [`DialogFsm::to_core_state`]
//! projects back to the core type when either of the two non-
//! terminal variants applies.

use smiths_core::DialogState as CoreDialogState;

/// Expanded dialog state with a terminal variant the FSM needs.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DialogFsmState {
    /// Dialog record created, ACK not yet observed.
    Early,
    /// ACK received (for 2xx) — call established.
    Confirmed,
    /// BYE exchanged, error, or CANCEL before ACK. No further
    /// events accepted on this FSM.
    Terminated,
}

/// Dialog-level events. Only the ones that drive state transitions
/// are modelled; ordinary in-dialog requests (re-INVITE, INFO, …)
/// don't change state and aren't represented here.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DialogEvent {
    /// ACK for the dialog's original 2xx arrived. Only valid in
    /// [`DialogFsmState::Early`].
    AckReceived,
    /// BYE exchange completed (received or sent). Terminates the
    /// dialog from any non-terminal state.
    ByeCompleted,
    /// `CANCEL` received before the 2xx was acknowledged, or a
    /// 3xx-6xx final came in that kills the dialog early. Only
    /// valid in [`DialogFsmState::Early`].
    Cancelled,
    /// Transport / dialog-invalidating error (e.g. peer disappeared,
    /// 5xx inside a dialog). Terminates from any non-terminal state.
    Error,
}

/// Reason an event was rejected. The FSM surfaces these rather than
/// silently absorbing — dialogs are long-lived, and an unexpected
/// event usually signals an application-layer bug worth logging.
#[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DialogTransitionError {
    /// The event isn't legal in the current state (e.g.
    /// `AckReceived` when already `Confirmed` or `Terminated`).
    #[error("dialog event {event:?} not legal in state {state:?}")]
    IllegalTransition {
        /// State the FSM was in when the event arrived.
        state: DialogFsmState,
        /// Event that was refused.
        event: DialogEvent,
    },
}

/// The dialog-layer state machine. Cheap to copy (just a state enum).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DialogFsm {
    state: DialogFsmState,
}

impl DialogFsm {
    /// Build a fresh FSM in [`DialogFsmState::Early`] — the state
    /// every dialog enters when the 2xx lands on the wire.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: DialogFsmState::Early,
        }
    }

    /// Current state.
    #[must_use]
    pub const fn state(&self) -> DialogFsmState {
        self.state
    }

    /// `true` if the FSM is in [`DialogFsmState::Terminated`].
    /// Dialog owners use this to decide when to drop the record.
    #[must_use]
    pub const fn is_terminated(&self) -> bool {
        matches!(self.state, DialogFsmState::Terminated)
    }

    /// Project to the serializable [`CoreDialogState`]. Returns
    /// `None` when the FSM is Terminated — the HA snapshot doesn't
    /// carry dead dialogs.
    #[must_use]
    pub const fn to_core_state(&self) -> Option<CoreDialogState> {
        match self.state {
            DialogFsmState::Early => Some(CoreDialogState::Early),
            DialogFsmState::Confirmed => Some(CoreDialogState::Confirmed),
            DialogFsmState::Terminated => None,
        }
    }

    /// Drive the FSM with one event. Returns the new state on
    /// success or a typed error describing the illegal transition.
    pub fn on_event(
        &mut self,
        event: DialogEvent,
    ) -> Result<DialogFsmState, DialogTransitionError> {
        use DialogEvent as E;
        use DialogFsmState as S;

        let next = match (self.state, event) {
            // --- Early → Confirmed on ACK --------------------------------
            (S::Early, E::AckReceived) => S::Confirmed,
            // --- Non-terminal → Terminated on BYE / Error / Cancelled ----
            // Cancelled is only legal pre-ACK (Early); BYE / Error
            // terminate from either Early or Confirmed.
            (S::Early, E::ByeCompleted | E::Error | E::Cancelled)
            | (S::Confirmed, E::ByeCompleted | E::Error) => S::Terminated,
            // --- Everything else is illegal ------------------------------
            _ => {
                return Err(DialogTransitionError::IllegalTransition {
                    state: self.state,
                    event,
                });
            }
        };
        self.state = next;
        Ok(next)
    }
}

impl Default for DialogFsm {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_starts_in_early() {
        let fsm = DialogFsm::new();
        assert_eq!(fsm.state(), DialogFsmState::Early);
        assert!(!fsm.is_terminated());
    }

    #[test]
    fn ack_moves_early_to_confirmed() {
        let mut fsm = DialogFsm::new();
        let next = fsm.on_event(DialogEvent::AckReceived).unwrap();
        assert_eq!(next, DialogFsmState::Confirmed);
    }

    #[test]
    fn bye_from_early_terminates() {
        let mut fsm = DialogFsm::new();
        fsm.on_event(DialogEvent::ByeCompleted).unwrap();
        assert!(fsm.is_terminated());
    }

    #[test]
    fn bye_from_confirmed_terminates() {
        let mut fsm = DialogFsm::new();
        fsm.on_event(DialogEvent::AckReceived).unwrap();
        fsm.on_event(DialogEvent::ByeCompleted).unwrap();
        assert!(fsm.is_terminated());
    }

    #[test]
    fn cancel_from_early_terminates() {
        let mut fsm = DialogFsm::new();
        fsm.on_event(DialogEvent::Cancelled).unwrap();
        assert!(fsm.is_terminated());
    }

    #[test]
    fn error_from_any_non_terminal_terminates() {
        let mut fsm = DialogFsm::new();
        fsm.on_event(DialogEvent::Error).unwrap();
        assert!(fsm.is_terminated());

        let mut fsm = DialogFsm::new();
        fsm.on_event(DialogEvent::AckReceived).unwrap();
        fsm.on_event(DialogEvent::Error).unwrap();
        assert!(fsm.is_terminated());
    }

    #[test]
    fn cancel_in_confirmed_is_illegal() {
        let mut fsm = DialogFsm::new();
        fsm.on_event(DialogEvent::AckReceived).unwrap();
        let err = fsm.on_event(DialogEvent::Cancelled).unwrap_err();
        assert!(matches!(
            err,
            DialogTransitionError::IllegalTransition {
                state: DialogFsmState::Confirmed,
                event: DialogEvent::Cancelled,
            }
        ));
    }

    #[test]
    fn ack_twice_is_illegal() {
        // Real UAs occasionally retransmit ACK. Our transaction
        // FSM absorbs ACK retransmits; by the time an event reaches
        // the dialog FSM it should be the first ACK. A second call
        // here is an application bug.
        let mut fsm = DialogFsm::new();
        fsm.on_event(DialogEvent::AckReceived).unwrap();
        let err = fsm.on_event(DialogEvent::AckReceived).unwrap_err();
        assert!(matches!(
            err,
            DialogTransitionError::IllegalTransition { .. }
        ));
    }

    #[test]
    fn events_in_terminated_are_illegal() {
        let mut fsm = DialogFsm::new();
        fsm.on_event(DialogEvent::ByeCompleted).unwrap();
        for ev in [
            DialogEvent::AckReceived,
            DialogEvent::ByeCompleted,
            DialogEvent::Cancelled,
            DialogEvent::Error,
        ] {
            assert!(
                fsm.on_event(ev).is_err(),
                "{ev:?} must be refused in Terminated"
            );
        }
    }

    #[test]
    fn core_state_projection() {
        let mut fsm = DialogFsm::new();
        assert_eq!(fsm.to_core_state(), Some(CoreDialogState::Early));
        fsm.on_event(DialogEvent::AckReceived).unwrap();
        assert_eq!(fsm.to_core_state(), Some(CoreDialogState::Confirmed));
        fsm.on_event(DialogEvent::ByeCompleted).unwrap();
        assert_eq!(fsm.to_core_state(), None);
    }
}
