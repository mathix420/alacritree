//! Which modal may act on the input events reaching the current frame.
//!
//! egui hands a frame the key presses produced while the *previous* frame was
//! on screen.  A modal that opens and reads keys in the same frame therefore
//! reads presses aimed at whatever the user was actually looking at, so an
//! Enter meant for a shell can confirm a dialog that had not appeared yet.
//! The same hazard applies when a modal's content changes: the worktree-create
//! dialog's done screen would otherwise act on a key pressed while its running
//! screen was up.
//!
//! Both cases are one rule: a modal may act on a frame's events only when the
//! previous frame painted that same modal showing the same thing.  Every
//! distinct thing a modal can show is its own [`ModalKind`], so the phases of
//! the create dialog are as separate here as two unrelated dialogs are.

use std::cell::Cell;

/// A modal, at the granularity the gate compares.  Two states a user must be
/// able to tell apart before answering are two kinds, not one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ModalKind {
    Delete,
    CloseSession,
    DetachAll,
    RemoveProject,
    Error,
    Palette,
    Rename,
    BaseBranchPicker,
    CreatePrompt,
    CreateRunning,
    CreateDone,
    Quit,
}

/// Remembers what the last frame painted so this frame's modal can tell
/// whether the events it is being handed were aimed at it.
///
/// The cells are what let a dialog painting behind `&self` still record
/// itself, and what keeps the call identical at a site that already holds a
/// mutable borrow of the state it is drawing.
#[derive(Default)]
pub(crate) struct ModalGate {
    painted: Cell<Option<ModalKind>>,
    current: Cell<Option<ModalKind>>,
}

impl ModalGate {
    /// Whether the modal now painting may act on this frame's input events.
    /// Records it as this frame's modal either way, so the caller cannot
    /// accept input without also arming the next frame's gate.
    pub(crate) fn accepts(&self, now: ModalKind) -> bool {
        self.current.set(Some(now));
        self.painted.get() == Some(now)
    }

    /// Carry what this frame painted into the next frame's gate.  Runs every
    /// frame, including the frames that paint no modal at all.
    pub(crate) fn end_frame(&self) {
        self.painted.set(self.current.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frame a modal opens on holds keys pressed against whatever was on
    /// screen before it.
    #[test]
    fn a_modal_refuses_the_frame_it_opens_on() {
        let gate = ModalGate::default();
        assert!(!gate.accepts(ModalKind::Delete));
    }

    #[test]
    fn a_modal_accepts_once_the_user_has_seen_it() {
        let gate = ModalGate::default();
        gate.accepts(ModalKind::Delete);
        gate.end_frame();
        assert!(gate.accepts(ModalKind::Delete));
    }

    /// The create dialog's screens replace each other without the modal ever
    /// closing, so the arriving screen must refuse the departing one's keys.
    #[test]
    fn a_changed_screen_refuses_the_previous_screens_keys() {
        let gate = ModalGate::default();
        gate.accepts(ModalKind::CreateRunning);
        gate.end_frame();
        assert!(!gate.accepts(ModalKind::CreateDone));
        gate.end_frame();
        assert!(gate.accepts(ModalKind::CreateDone));
    }

    /// Closing one dialog and opening another in consecutive frames must not
    /// let the second inherit the first's permission.
    #[test]
    fn a_replacing_modal_starts_from_refused() {
        let gate = ModalGate::default();
        gate.accepts(ModalKind::Delete);
        gate.end_frame();
        assert!(!gate.accepts(ModalKind::Quit));
    }

    /// A frame that paints no modal clears the permission, so a dialog
    /// reopened later does not accept the keys that closed it.
    #[test]
    fn a_frame_without_a_modal_clears_the_gate() {
        let gate = ModalGate::default();
        gate.accepts(ModalKind::Delete);
        gate.end_frame();
        gate.end_frame();
        assert!(!gate.accepts(ModalKind::Delete));
    }
}
