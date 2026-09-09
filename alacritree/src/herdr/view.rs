//! Which herdr pane alacritree is showing, and when to tell herdr to move.
//!
//! Every herdr client draws the one pane herdr has focused, so a session
//! sharing herdr's view has to ask for its own pane before its client can
//! start, and follows herdr afterwards.

use std::time::Instant;

use crate::config::AttachMode;
use crate::jobs;
use crate::session::SessionId;

use super::{attaches_directly, Agent, HerdrKey, Side};

/// The shared view herdr is being pointed at, and the call doing the
/// pointing.  The handle is held rather than dropped because dropping a job
/// cancels it.
pub struct HerdrViewFocus {
    pub session: SessionId,
    pub job: jobs::Job<Result<(), String>>,
}

#[derive(Default)]
pub struct HerdrViewSync {
    pub visible: Option<SessionId>,
    pub focused: Option<SessionId>,
    follow_after: Option<Instant>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum HerdrViewAction {
    Focus(SessionId),
    Follow(HerdrKey),
}

impl HerdrViewSync {
    pub fn closed(&mut self, id: SessionId) {
        if self.visible == Some(id) {
            self.visible = None;
            self.follow_after = None;
        }
        if self.focused == Some(id) {
            self.focused = None;
        }
    }

    pub fn attached(&mut self, id: SessionId, at: Instant) {
        self.visible = Some(id);
        self.settled(id, true, at);
    }

    pub fn settled(&mut self, id: SessionId, succeeded: bool, at: Instant) {
        if self.visible == Some(id) {
            self.focused = Some(id);
            self.follow_after = succeeded.then_some(at);
        }
    }

    pub fn next(
        &mut self,
        active: Option<(SessionId, &HerdrKey, bool)>,
        attach: AttachMode,
        snapshot: Option<(Instant, &Side, &[Agent])>,
        busy: bool,
    ) -> Option<HerdrViewAction> {
        let active = active
            .filter(|(_, key, has_agent)| !attaches_directly(&key.side, attach, *has_agent));
        let visible = active.map(|(id, ..)| id);
        if self.visible != visible {
            self.visible = visible;
            self.focused = None;
            self.follow_after = None;
        }
        if busy {
            return None;
        }
        let (id, key, has_agent) = active?;
        if needs_view_focus(Some(key), attach, has_agent, id, self.focused) {
            return Some(HerdrViewAction::Focus(id));
        }
        let (sampled_at, side, agents) = snapshot?;
        if side != &key.side || sampled_at <= self.follow_after? {
            return None;
        }
        let focused = agents.iter().find(|agent| agent.focused)?;
        self.follow_after = Some(sampled_at);
        (focused.terminal_id != key.terminal_id).then(|| {
            HerdrViewAction::Follow(HerdrKey {
                side: side.clone(),
                terminal_id: focused.terminal_id.clone(),
            })
        })
    }
}

/// Whether the session on screen still owes herdr a focus call.  A direct
/// attach draws its own pane whatever herdr focuses, and an ordinary shell
/// has no pane at all, so neither ever asks.
pub fn needs_view_focus(
    key: Option<&HerdrKey>,
    attach: AttachMode,
    has_agent: bool,
    active: SessionId,
    focused: Option<SessionId>,
) -> bool {
    key.is_some_and(|key| !attaches_directly(&key.side, attach, has_agent))
        && focused != Some(active)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use crate::herdr;

    #[test]
    fn herdr_shared_view_follows_new_tabs_and_refocuses_on_return() {
        let side = herdr::Side::Native;
        let t1 = herdr::HerdrKey { side: side.clone(), terminal_id: "t1".into() };
        let t2 = herdr::HerdrKey { side: side.clone(), terminal_id: "t2".into() };
        let panes = herdr::Listing::Panes.parse(
            r#"{"result":{"panes":[
                {"terminal_id":"t1","pane_id":"w1:p1","tab_id":"w1:t1","focused":false},
                {"terminal_id":"t2","pane_id":"w2:p1","tab_id":"w2:t1","focused":true}
            ]}}"#,
        );
        let mut sync = HerdrViewSync::default();
        let active = Some((1, &t1, false));
        assert_eq!(
            sync.next(active, AttachMode::Session, None, false),
            Some(HerdrViewAction::Focus(1))
        );
        let focused_at = Instant::now();
        sync.settled(1, true, focused_at);
        let snapshot = Some((focused_at + Duration::from_millis(1), &side, panes.as_slice()));
        assert_eq!(
            sync.next(active, AttachMode::Session, snapshot, false),
            Some(HerdrViewAction::Follow(t2.clone()))
        );
        sync.attached(2, focused_at + Duration::from_millis(2));
        assert_eq!(sync.next(Some((2, &t2, false)), AttachMode::Session, snapshot, false), None);
        assert_eq!(sync.next(None, AttachMode::Session, snapshot, false), None);
        assert_eq!(
            sync.next(active, AttachMode::Session, snapshot, false),
            Some(HerdrViewAction::Focus(1))
        );
    }

    #[test]
    fn herdr_shared_view_refocuses_after_an_ordinary_session() {
        let key = herdr::HerdrKey { side: herdr::Side::Native, terminal_id: "t1".into() };
        let mut sync = HerdrViewSync::default();
        sync.attached(1, Instant::now());
        assert_eq!(sync.next(None, AttachMode::Session, None, false), None);
        assert_eq!(
            sync.next(Some((1, &key, false)), AttachMode::Session, None, false),
            Some(HerdrViewAction::Focus(1))
        );
    }

    #[test]
    fn herdr_follow_attempts_wait_for_a_new_snapshot() {
        let side = herdr::Side::Native;
        let key = herdr::HerdrKey { side: side.clone(), terminal_id: "t1".into() };
        let panes = herdr::Listing::Panes.parse(
            r#"{"result":{"panes":[
                {"terminal_id":"t2","pane_id":"w2:p1","tab_id":"w2:t1","focused":true}
            ]}}"#,
        );
        let mut sync = HerdrViewSync::default();
        let focused_at = Instant::now();
        sync.attached(1, focused_at);
        let active = Some((1, &key, false));
        let snapshot = Some((focused_at + Duration::from_millis(1), &side, panes.as_slice()));
        assert!(matches!(
            sync.next(active, AttachMode::Session, snapshot, false),
            Some(HerdrViewAction::Follow(_))
        ));
        assert_eq!(sync.next(active, AttachMode::Session, snapshot, false), None);
        let snapshot = Some((focused_at + Duration::from_millis(2), &side, panes.as_slice()));
        assert!(matches!(
            sync.next(active, AttachMode::Session, snapshot, false),
            Some(HerdrViewAction::Follow(_))
        ));
        sync.settled(1, false, focused_at + Duration::from_millis(3));
        let snapshot = Some((focused_at + Duration::from_millis(4), &side, panes.as_slice()));
        assert_eq!(sync.next(active, AttachMode::Session, snapshot, false), None);
    }

    #[test]
    fn herdr_shared_view_rejects_stale_and_foreign_focus_snapshots() {
        let side = herdr::Side::Wsl("ubuntu".into());
        let other_side = herdr::Side::Wsl("debian".into());
        let key = herdr::HerdrKey { side: side.clone(), terminal_id: "t1".into() };
        let panes = herdr::Listing::Panes.parse(
            r#"{"result":{"panes":[
                {"terminal_id":"t2","pane_id":"w2:p1","tab_id":"w2:t1","focused":true}
            ]}}"#,
        );
        let mut sync = HerdrViewSync::default();
        let active = Some((1, &key, false));
        let started = Instant::now();
        assert_eq!(
            sync.next(active, AttachMode::Agent, None, false),
            Some(HerdrViewAction::Focus(1))
        );
        assert_eq!(sync.next(active, AttachMode::Agent, None, true), None);
        let settled = started + Duration::from_millis(1);
        sync.settled(1, true, settled);
        assert_eq!(
            sync.next(active, AttachMode::Agent, Some((started, &side, &panes)), false),
            None
        );
        let fresh = settled + Duration::from_millis(1);
        assert_eq!(
            sync.next(active, AttachMode::Agent, Some((fresh, &other_side, &panes)), false),
            None
        );
        assert_eq!(sync.next(active, AttachMode::Agent, Some((fresh, &side, &panes)), true), None);
        assert_eq!(
            sync.next(
                Some((1, &key, true)),
                AttachMode::Agent,
                Some((fresh, &side, &panes)),
                false
            ),
            None
        );
        assert_eq!(
            sync.next(active, AttachMode::Agent, Some((fresh, &side, &panes)), false),
            Some(HerdrViewAction::Focus(1))
        );
    }

    #[test]
    fn herdr_focus_completion_cannot_restore_a_view_left_while_pending() {
        let key = herdr::HerdrKey { side: herdr::Side::Native, terminal_id: "t1".into() };
        let active = Some((1, &key, false));
        let mut sync = HerdrViewSync::default();
        assert_eq!(
            sync.next(active, AttachMode::Session, None, false),
            Some(HerdrViewAction::Focus(1))
        );
        assert_eq!(sync.next(None, AttachMode::Session, None, true), None);
        sync.settled(1, true, Instant::now());
        assert_eq!(
            sync.next(active, AttachMode::Session, None, false),
            Some(HerdrViewAction::Focus(1))
        );
        sync.settled(1, false, Instant::now());
        assert_eq!(sync.next(active, AttachMode::Session, None, false), None);
    }

    /// A shared view shows whatever pane herdr has focused, so the one on
    /// screen has to keep asking for its own.
    #[test]
    fn a_shared_view_asks_herdr_for_its_pane() {
        let key = herdr::HerdrKey { side: herdr::Side::Native, terminal_id: "t1".into() };
        let asks = needs_view_focus(Some(&key), AttachMode::Agent, true, 1, None);
        assert_eq!(asks, cfg!(windows));
    }

    /// A direct attach is wired to one pane, so herdr's focus decides nothing
    /// about what it draws.
    #[test]
    fn a_direct_attach_never_asks_herdr_for_its_pane() {
        let key = herdr::HerdrKey { side: herdr::Side::Wsl("d".into()), terminal_id: "t1".into() };
        assert!(!needs_view_focus(Some(&key), AttachMode::Agent, true, 1, None));
        assert!(!needs_view_focus(None, AttachMode::Agent, true, 1, None));
    }

    /// The pane herdr was last pointed at is where it still is, and asking
    /// again every frame would spawn a herdr per frame.
    #[test]
    fn a_shared_view_asks_once_per_switch() {
        let key = herdr::HerdrKey { side: herdr::Side::Native, terminal_id: "t1".into() };
        assert!(!needs_view_focus(Some(&key), AttachMode::Agent, true, 1, Some(1)));
        let asks = needs_view_focus(Some(&key), AttachMode::Agent, true, 2, Some(1));
        assert_eq!(asks, cfg!(windows));
    }
}
