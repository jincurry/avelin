//! Presentation rules for one operation. No GPUI or executor is initialized here.
//!
//! Commands express intent. Only coordinator snapshots confirm execution results.
//! Demonstration transitions are explicit and cannot advance a connected operation.

use crate::{UiPhase, UiResponse, UiSnapshot};
use avelin_core::Operation;

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) enum LaunchStatus {
    #[default]
    Idle,
    Pending,
    Succeeded,
    Failed(String),
    Unavailable,
}

pub(crate) struct UiState {
    snapshot: UiSnapshot,
    connected: bool,
    start_requested: bool,
    cancel_requested: bool,
    disconnected: bool,
    operation: Operation,
    responses_open: bool,
    launch_status: LaunchStatus,
}

impl UiState {
    #[cfg(test)]
    pub(crate) fn new(connected: bool) -> Self {
        Self::for_operation(connected, Operation::Install)
    }

    pub(crate) fn for_operation(connected: bool, operation: Operation) -> Self {
        Self {
            snapshot: UiSnapshot::default(),
            connected,
            start_requested: false,
            cancel_requested: false,
            disconnected: false,
            operation,
            responses_open: connected,
            launch_status: LaunchStatus::Idle,
        }
    }

    pub(crate) fn snapshot(&self) -> &UiSnapshot {
        &self.snapshot
    }

    pub(crate) fn phase(&self) -> UiPhase {
        // A confirmed terminal result or a reported commit phase takes priority
        // over local intent. Cancelling cannot promise to interrupt committing.
        if self.snapshot.phase.is_terminal() || self.snapshot.phase == UiPhase::Finalizing {
            self.snapshot.phase
        } else if self.cancel_requested {
            UiPhase::Cancelling
        } else if self.awaiting_start() {
            UiPhase::Preparing
        } else {
            self.snapshot.phase
        }
    }

    pub(crate) fn awaiting_start(&self) -> bool {
        self.start_requested && self.snapshot.phase == UiPhase::Ready && !self.cancel_requested
    }

    pub(crate) fn can_start(&self, accepted: bool) -> bool {
        accepted && !self.disconnected && !self.start_requested && self.phase() == UiPhase::Ready
    }

    /// Returns true once, when the view should dispatch a start command.
    pub(crate) fn request_start(&mut self, accepted: bool) -> bool {
        if !self.can_start(accepted) {
            return false;
        }
        self.start_requested = true;
        if !self.connected {
            self.snapshot = UiSnapshot {
                phase: UiPhase::Preparing,
                total: Some(100),
                ..UiSnapshot::default()
            };
        }
        true
    }

    pub(crate) fn can_cancel(&self) -> bool {
        self.operation == Operation::Install
            && !self.disconnected
            && matches!(self.phase(), UiPhase::Preparing | UiPhase::Installing)
    }

    pub(crate) fn can_launch(&self) -> bool {
        self.connected
            && self.responses_open
            && matches!(self.operation, Operation::Install | Operation::Upgrade)
            && self.phase() == UiPhase::Succeeded
            && matches!(
                self.launch_status,
                LaunchStatus::Idle | LaunchStatus::Failed(_)
            )
    }

    pub(crate) fn launch_status(&self) -> &LaunchStatus {
        &self.launch_status
    }

    pub(crate) fn request_launch(&mut self) -> bool {
        if !self.can_launch() {
            return false;
        }
        self.launch_status = LaunchStatus::Pending;
        true
    }

    pub(crate) fn receive_response(&mut self, response: UiResponse) -> bool {
        if self.launch_status != LaunchStatus::Pending {
            return false;
        }
        let UiResponse::LaunchResult { succeeded, detail } = response;
        self.launch_status = if succeeded {
            LaunchStatus::Succeeded
        } else {
            LaunchStatus::Failed(detail)
        };
        true
    }

    pub(crate) fn close_responses(&mut self) -> bool {
        if !self.responses_open {
            return false;
        }
        self.responses_open = false;
        if self.launch_status != LaunchStatus::Succeeded {
            self.launch_status = LaunchStatus::Unavailable;
        }
        true
    }

    /// Sending a cancellation request never establishes a cancelled result.
    pub(crate) fn request_cancel(&mut self) -> bool {
        if !self.can_cancel() {
            return false;
        }
        self.cancel_requested = true;
        if !self.connected {
            self.snapshot.phase = UiPhase::Cancelling;
        }
        true
    }

    pub(crate) fn may_close(&self) -> bool {
        !self.phase().is_running()
    }

    pub(crate) fn disconnected(&self) -> bool {
        self.disconnected
    }

    pub(crate) fn receive(&mut self, snapshot: UiSnapshot) -> bool {
        // The first terminal result is immutable for this single-operation view.
        // In particular, Unknown cannot become success or enable an automatic retry.
        if !self.connected || self.disconnected || self.snapshot.phase.is_terminal() {
            return false;
        }
        self.snapshot = snapshot;
        true
    }

    /// Covers a dropped snapshot channel and an undeliverable command alike.
    /// A lost connection is neither a successful install nor confirmed cleanup.
    pub(crate) fn disconnect(&mut self, detail: &str) -> bool {
        if !self.connected || self.disconnected {
            return false;
        }
        self.disconnected = true;
        if self.snapshot.phase.is_terminal() {
            return false;
        }
        self.snapshot.phase = UiPhase::Unknown;
        self.snapshot.detail = detail.into();
        true
    }

    pub(crate) fn advance_demo(&mut self) -> bool {
        if self.connected {
            return false;
        }
        match self.snapshot.phase {
            UiPhase::Preparing => self.snapshot.phase = UiPhase::Installing,
            UiPhase::Installing => {
                self.snapshot.completed = (self.snapshot.completed + 7).min(100);
                if self.snapshot.completed == 100 {
                    self.snapshot.phase = UiPhase::Finalizing;
                }
            }
            UiPhase::Cancelling => self.snapshot.phase = UiPhase::Cancelled,
            UiPhase::Finalizing => self.snapshot.phase = UiPhase::Succeeded,
            _ => return false,
        }
        true
    }

    pub(crate) fn reset_demo(&mut self) -> bool {
        if self.connected || !self.phase().is_terminal() {
            return false;
        }
        *self = Self::for_operation(false, self.operation);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uninstall_never_accepts_cancel_or_launch_even_after_success() {
        let mut state = UiState::for_operation(true, Operation::Uninstall);
        assert!(state.may_close());
        assert!(state.request_start(true));
        for phase in [UiPhase::Preparing, UiPhase::Installing, UiPhase::Finalizing] {
            state.receive(snapshot(phase));
            assert!(!state.request_cancel());
            assert!(!state.request_launch());
            assert!(!state.may_close());
        }
        state.receive(snapshot(UiPhase::Succeeded));
        assert!(state.may_close());
        assert!(!state.request_launch());
    }

    #[test]
    fn upgrade_waits_for_a_confirmed_terminal_without_cancel_and_launches_only_after_success() {
        for terminal in [UiPhase::Succeeded, UiPhase::Failed, UiPhase::Unknown] {
            let mut state = UiState::for_operation(true, Operation::Upgrade);
            assert!(state.may_close());
            assert!(state.request_start(true));
            for phase in [UiPhase::Preparing, UiPhase::Installing, UiPhase::Finalizing] {
                state.receive(snapshot(phase));
                assert!(!state.request_cancel());
                assert!(!state.request_launch());
                assert!(!state.may_close());
            }
            state.receive(snapshot(terminal));
            let confirmed = state.snapshot().clone();
            assert!(state.may_close());
            assert_eq!(state.request_launch(), terminal == UiPhase::Succeeded);
            assert!(!state.receive(snapshot(UiPhase::Installing)));
            assert_eq!(state.snapshot(), &confirmed);
        }
    }

    #[test]
    fn launch_response_requires_a_pending_action_and_preserves_terminal_snapshot() {
        let mut state = UiState::new(true);
        assert!(!state.request_launch());
        assert!(!state.receive_response(UiResponse::LaunchResult {
            succeeded: true,
            detail: String::new()
        }));
        state.receive(snapshot(UiPhase::Succeeded));
        let confirmed = state.snapshot().clone();
        assert!(state.request_launch());
        assert!(!state.request_launch());
        assert!(state.receive_response(UiResponse::LaunchResult {
            succeeded: false,
            detail: "access denied".into()
        }));
        assert_eq!(state.snapshot(), &confirmed);
        assert!(state.request_launch());
        assert!(state.receive_response(UiResponse::LaunchResult {
            succeeded: true,
            detail: "pid=1".into()
        }));
        assert_eq!(state.snapshot(), &confirmed);
        assert!(!state.request_launch());
        assert!(!state.receive_response(UiResponse::LaunchResult {
            succeeded: false,
            detail: "late reply".into()
        }));
        state.close_responses();
        assert_eq!(state.launch_status(), &LaunchStatus::Succeeded);
    }

    fn snapshot(phase: UiPhase) -> UiSnapshot {
        UiSnapshot {
            phase,
            completed: 21,
            total: Some(100),
            detail: format!("reported {phase:?}"),
        }
    }

    #[test]
    fn start_requires_consent_and_is_dispatched_only_once_while_waiting() {
        let mut state = UiState::new(true);
        assert!(state.may_close());
        assert!(!state.request_start(false));
        assert_eq!(state.phase(), UiPhase::Ready);
        assert!(state.request_start(true));
        assert!(!state.request_start(true));
        assert!(state.awaiting_start());
        assert_eq!(state.phase(), UiPhase::Preparing);
        assert_eq!(state.snapshot().phase, UiPhase::Ready);
        assert!(!state.may_close());
        state.receive(snapshot(UiPhase::Installing));
        assert!(!state.awaiting_start());
        assert!(!state.request_start(true));
        assert!(!state.may_close());
    }

    #[test]
    fn cancel_waits_through_in_flight_progress_until_a_reported_terminal() {
        let mut state = UiState::new(true);
        assert!(!state.request_cancel());
        state.request_start(true);
        assert!(state.request_cancel());
        assert!(!state.request_cancel());
        assert_eq!(state.snapshot().phase, UiPhase::Ready);
        for phase in [UiPhase::Ready, UiPhase::Preparing, UiPhase::Installing] {
            state.receive(snapshot(phase));
            assert_eq!(state.phase(), UiPhase::Cancelling);
            assert!(!state.may_close());
            assert!(!state.can_cancel());
        }
        state.receive(snapshot(UiPhase::Cancelled));
        assert_eq!(state.phase(), UiPhase::Cancelled);
        assert!(state.may_close());
    }

    #[test]
    fn reported_commit_overrides_a_pending_cancel_without_inventing_its_result() {
        for terminal in [UiPhase::Succeeded, UiPhase::Failed, UiPhase::Unknown] {
            let mut state = UiState::new(true);
            state.request_start(true);
            state.request_cancel();
            state.receive(snapshot(UiPhase::Finalizing));
            assert_eq!(state.phase(), UiPhase::Finalizing);
            assert!(!state.can_cancel());
            assert!(!state.request_cancel());
            assert!(!state.may_close());
            state.receive(snapshot(terminal));
            assert_eq!(state.phase(), terminal);
        }
    }

    #[test]
    fn every_first_terminal_is_immutable_even_after_conflicting_results_and_disconnect() {
        for terminal in [
            UiPhase::Succeeded,
            UiPhase::Failed,
            UiPhase::Cancelled,
            UiPhase::Unknown,
        ] {
            let mut state = UiState::new(true);
            let report = snapshot(terminal);
            let detail = report.detail.clone();
            state.receive(report);
            for later in [
                UiPhase::Installing,
                UiPhase::Ready,
                UiPhase::Succeeded,
                UiPhase::Unknown,
            ] {
                assert!(!state.receive(snapshot(later)));
                assert_eq!(state.phase(), terminal);
                assert_eq!(state.snapshot().detail, detail);
            }
            assert!(!state.disconnect("late disconnect"));
            assert_eq!(state.phase(), terminal);
            assert!(state.may_close());
            assert!(!state.request_start(true));
            assert!(!state.request_cancel());
            assert!(!state.reset_demo());
        }
    }

    #[test]
    fn lost_channel_or_delivery_is_unknown_in_every_nonterminal_phase() {
        for phase in [
            UiPhase::Ready,
            UiPhase::Preparing,
            UiPhase::Installing,
            UiPhase::Cancelling,
            UiPhase::Finalizing,
        ] {
            let mut state = UiState::new(true);
            state.receive(snapshot(phase));
            assert!(state.disconnect("result not confirmed"));
            assert!(state.disconnected());
            assert_eq!(state.phase(), UiPhase::Unknown);
            assert_eq!(state.snapshot().detail, "result not confirmed");
            assert!(state.may_close());
            assert!(!state.request_start(true));
            assert!(!state.receive(snapshot(UiPhase::Succeeded)));
            assert!(!state.disconnect("replacement"));
            assert_eq!(state.snapshot().detail, "result not confirmed");
        }
    }

    #[test]
    fn failed_start_or_cancel_delivery_cannot_be_masked_by_pending_intent() {
        for cancel in [false, true] {
            let mut state = UiState::new(true);
            assert!(state.request_start(true));
            if cancel {
                assert!(state.request_cancel());
            }
            assert!(state.disconnect("command delivery failed"));
            assert_eq!(state.phase(), UiPhase::Unknown);
            assert!(!state.awaiting_start());
            assert!(!state.request_start(true));
            assert!(!state.request_cancel());
            assert!(!state.receive(snapshot(UiPhase::Succeeded)));
            assert_eq!(state.snapshot().detail, "command delivery failed");
        }
    }

    #[test]
    fn connected_operations_cannot_advance_or_reset_using_demo_actions() {
        let mut state = UiState::new(true);
        state.request_start(true);
        for _ in 0..30 {
            assert!(!state.advance_demo());
        }
        assert_eq!(state.phase(), UiPhase::Preparing);
        assert!(!state.reset_demo());
        state.disconnect("unknown");
        assert!(!state.reset_demo());
        assert_eq!(state.phase(), UiPhase::Unknown);
    }

    #[test]
    fn demo_completes_only_on_its_explicit_ticks_and_resets_after_terminal() {
        let mut state = UiState::new(false);
        assert!(!state.advance_demo());
        assert!(!state.receive(snapshot(UiPhase::Succeeded)));
        assert!(state.request_start(true));
        assert!(!state.reset_demo());
        assert!(state.advance_demo());
        for _ in 0..15 {
            assert!(state.advance_demo());
        }
        assert_eq!(state.phase(), UiPhase::Finalizing);
        assert_eq!(state.snapshot().completed, 100);
        assert!(!state.request_cancel());
        assert!(state.advance_demo());
        assert_eq!(state.phase(), UiPhase::Succeeded);
        assert!(!state.advance_demo());
        assert!(state.reset_demo());
        assert_eq!(state.phase(), UiPhase::Ready);
        assert!(state.request_start(true));
        assert!(state.request_cancel());
        assert_eq!(state.phase(), UiPhase::Cancelling);
        assert!(state.advance_demo());
        assert_eq!(state.phase(), UiPhase::Cancelled);
        assert!(state.reset_demo());
    }
}
