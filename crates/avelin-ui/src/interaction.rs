//! Small, window-independent guards for asynchronous UI intent.

use crate::UiPhase;

#[derive(Default)]
pub(crate) struct DirectoryPicker {
    last_request: u64,
    pending: Option<u64>,
}

impl DirectoryPicker {
    pub(crate) fn pending(&self) -> bool {
        self.pending.is_some()
    }

    pub(crate) fn can_edit(&self, phase: UiPhase) -> bool {
        phase == UiPhase::Ready && !self.pending()
    }

    pub(crate) fn begin(&mut self, phase: UiPhase) -> Option<u64> {
        if !self.can_edit(phase) {
            return None;
        }
        self.last_request = self.last_request.checked_add(1)?;
        self.pending = Some(self.last_request);
        self.pending
    }

    /// A stale callback cannot clear a newer request or mutate the view. A
    /// matching callback releases its guard even if execution has since begun.
    pub(crate) fn finish(&mut self, request: u64, phase: UiPhase) -> bool {
        if self.pending != Some(request) {
            return false;
        }
        self.pending = None;
        phase == UiPhase::Ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_picker_blocks_edits_and_duplicate_requests_until_it_finishes() {
        let mut picker = DirectoryPicker::default();
        assert!(picker.can_edit(UiPhase::Ready));
        let request = picker.begin(UiPhase::Ready).unwrap();
        assert!(picker.pending());
        assert!(!picker.can_edit(UiPhase::Ready));
        assert_eq!(picker.begin(UiPhase::Ready), None);
        assert!(picker.finish(request, UiPhase::Ready));
        assert!(picker.can_edit(UiPhase::Ready));
        assert!(!picker.finish(request, UiPhase::Ready));
    }

    #[test]
    fn stale_callback_cannot_release_a_newer_picker() {
        let mut picker = DirectoryPicker::default();
        let first = picker.begin(UiPhase::Ready).unwrap();
        assert!(picker.finish(first, UiPhase::Ready));
        let second = picker.begin(UiPhase::Ready).unwrap();
        assert_ne!(first, second);
        assert!(!picker.finish(first, UiPhase::Ready));
        assert!(picker.pending());
        assert!(picker.finish(second, UiPhase::Ready));
    }

    #[test]
    fn every_nonready_phase_rejects_new_pickers_and_late_results() {
        for phase in [
            UiPhase::Preparing,
            UiPhase::Installing,
            UiPhase::Cancelling,
            UiPhase::Finalizing,
            UiPhase::Succeeded,
            UiPhase::Failed,
            UiPhase::Cancelled,
            UiPhase::Unknown,
        ] {
            let mut picker = DirectoryPicker::default();
            assert!(!picker.can_edit(phase));
            assert_eq!(picker.begin(phase), None);
            let request = picker.begin(UiPhase::Ready).unwrap();
            assert!(!picker.finish(request, phase));
            assert!(!picker.pending());
            assert!(!picker.can_edit(phase));
        }
    }
}
