//! Which herdr pane alacritree is showing, and when to tell herdr to move.
//!
//! Every herdr client draws the one pane herdr has focused, so a session
//! sharing herdr's view has to ask for its own pane before its client can
//! start, and follows herdr afterwards.

use std::collections::HashMap;
use std::time::Instant;

use crate::config::{AttachMode, FollowFocus};
use crate::herdr::EndpointCache;
use crate::jobs;
use crate::session::SessionId;

use super::{HerdrKey, Side, attaches_directly};

/// Where a side's focus was last established, and when.  The stamp is a
/// watermark: a listing sampled at or before it cannot form an edge, so one
/// already in flight when alacritree moved herdr's focus cannot run the
/// change backwards.
struct TrailEntry {
    terminal_id: String,
    stamped_at: Instant,
}

/// The shared view herdr is being pointed at, and the call doing the
/// pointing.  The handle is held rather than dropped because dropping a job
/// cancels it.
pub struct HerdrViewFocus {
    pub session: SessionId,
    /// The pane the job's own focus call targets, for stamping the trail on
    /// success with the pane herdr is actually on rather than whatever
    /// session happens to be active when the job completes.
    pub key: HerdrKey,
    pub job: jobs::Job<Result<(), String>>,
}

#[derive(Default)]
pub struct HerdrViewSync {
    pub visible: Option<SessionId>,
    pub focused: Option<SessionId>,
    follow_after: Option<Instant>,
    trail: HashMap<Side, TrailEntry>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum HerdrViewAction {
    Focus(SessionId),
    Follow(HerdrKey),
}

/// Everything `next` decides from, in one struct because there are more of
/// them than a positional call can carry legibly and clippy allows.
pub struct ViewInputs<'a> {
    /// The active session and its herdr key, before any direct-attach filter.
    /// The filter belongs to the shared-view path: a session that attaches
    /// directly still occupies a pane, and a trail blind to its key would
    /// follow the user to the pane they are already on.
    pub active: Option<(SessionId, Option<&'a HerdrKey>, bool)>,
    pub attach: AttachMode,
    pub follow: FollowFocus,
    pub caches: &'a [EndpointCache],
    /// The window is focused, the terminal has pane focus, and neither a
    /// modal nor the palette is open.
    pub attentive: bool,
    pub busy: bool,
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

    pub fn attached(&mut self, id: SessionId, key: Option<&HerdrKey>, at: Instant) {
        if let Some(key) = key {
            self.moved_focus(key, at);
        }
        self.visible = Some(id);
        self.settled(id, true, at);
    }

    pub fn settled(&mut self, id: SessionId, succeeded: bool, at: Instant) {
        if self.visible == Some(id) {
            self.focused = Some(id);
            self.follow_after = succeeded.then_some(at);
        }
    }

    /// Record where herdr's focus now is, without proposing anything.  Every
    /// path that moves herdr's focus calls this, so the move alacritree asked
    /// for is never mistaken for one the user made inside herdr.
    pub fn moved_focus(&mut self, key: &HerdrKey, at: Instant) {
        self.trail.insert(key.side.clone(), TrailEntry {
            terminal_id: key.terminal_id.clone(),
            stamped_at: at,
        });
    }

    /// The first side whose focused pane differs from what the trail holds.
    /// Two sides changing between one frame and the next is rare enough that
    /// a deliberate tiebreak would be inventing a rule nobody can observe,
    /// and the other side's change is still a change on the next frame.
    fn trail_edge(&mut self, inputs: &ViewInputs<'_>) -> Option<HerdrKey> {
        let live: Vec<&Side> = inputs.caches.iter().map(EndpointCache::side).collect();
        self.trail.retain(|side, _| live.contains(&side));
        let active_key = inputs.active.and_then(|(_, key, _)| key);
        let mut edge = None;
        for cache in inputs.caches {
            let Some(sampled_at) = cache.sampled_at() else { continue };
            // Under `show_panes = false` an unlisted focus reads the same as
            // a failed poll, so a side with no focused pane is left alone.
            let Some(focused) = cache.agents().iter().find(|agent| agent.focused) else {
                continue;
            };
            let key =
                HerdrKey { side: cache.side().clone(), terminal_id: focused.terminal_id.clone() };
            match self.trail.get(cache.side()) {
                // An id appearing where there was no entry is first sight.
                None => self.moved_focus(&key, sampled_at),
                Some(entry) => {
                    if entry.terminal_id == focused.terminal_id || sampled_at <= entry.stamped_at {
                        continue;
                    }
                    if active_key == Some(&key) {
                        self.moved_focus(&key, sampled_at);
                        continue;
                    }
                    edge.get_or_insert(key);
                },
            }
        }
        edge
    }

    pub fn next(&mut self, inputs: ViewInputs<'_>) -> Option<HerdrViewAction> {
        if let Some(action) = self.shared_view(&inputs) {
            return match action {
                // The setting governs whether herdr may move alacritree,
                // never whether alacritree may move herdr: a shared view
                // draws the wrong pane without its own focus call.
                HerdrViewAction::Follow(_) if inputs.follow == FollowFocus::Off => None,
                action => Some(action),
            };
        }
        if inputs.busy || !inputs.attentive {
            return None;
        }
        let edge = self.trail_edge(&inputs)?;
        (inputs.follow == FollowFocus::Always).then_some(HerdrViewAction::Follow(edge))
    }

    /// The session on screen asking herdr for its own pane, and following
    /// herdr afterwards.  Watermarked against listings that were already in
    /// flight when our own focus call landed.
    fn shared_view(&mut self, inputs: &ViewInputs<'_>) -> Option<HerdrViewAction> {
        let active = inputs
            .active
            .and_then(|(id, key, has_agent)| Some((id, key?, has_agent)))
            .filter(|(_, key, has_agent)| !attaches_directly(&key.side, inputs.attach, *has_agent));
        let visible = active.map(|(id, ..)| id);
        if self.visible != visible {
            self.visible = visible;
            self.focused = None;
            self.follow_after = None;
        }
        if inputs.busy {
            return None;
        }
        let (id, key, has_agent) = active?;
        if needs_view_focus(Some(key), inputs.attach, has_agent, id, self.focused) {
            return Some(HerdrViewAction::Focus(id));
        }
        let cache = inputs.caches.iter().find(|cache| cache.side() == &key.side)?;
        let sampled_at = cache.sampled_at()?;
        if !inputs.attentive || sampled_at <= self.follow_after? {
            return None;
        }
        let focused = cache.agents().iter().find(|agent| agent.focused)?;
        self.follow_after = Some(sampled_at);
        (focused.terminal_id != key.terminal_id).then(|| {
            HerdrViewAction::Follow(HerdrKey {
                side: key.side.clone(),
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

    fn inputs<'a>(
        active: Option<(SessionId, Option<&'a HerdrKey>, bool)>,
        attach: AttachMode,
        caches: &'a [herdr::EndpointCache],
        busy: bool,
    ) -> ViewInputs<'a> {
        ViewInputs { active, attach, follow: FollowFocus::Herdr, caches, attentive: true, busy }
    }

    fn always<'a>(
        active: Option<(SessionId, Option<&'a HerdrKey>, bool)>,
        caches: &'a [herdr::EndpointCache],
    ) -> ViewInputs<'a> {
        ViewInputs {
            active,
            attach: AttachMode::Session,
            follow: FollowFocus::Always,
            caches,
            attentive: true,
            busy: false,
        }
    }

    fn one_focused(
        side: &herdr::Side,
        terminal_id: &str,
        at: Instant,
    ) -> Vec<herdr::EndpointCache> {
        let panes = herdr::Listing::Panes.parse(&format!(
            r#"{{"result":{{"panes":[
                {{"terminal_id":"{terminal_id}","pane_id":"w1:p1","tab_id":"w1:t1","focused":true}}
            ]}}}}"#
        ));
        vec![herdr::EndpointCache::for_test(side.clone(), panes, at)]
    }

    /// Starting alacritree must never yank the user somewhere, so the first
    /// reading of a side is a baseline rather than a change.
    #[test]
    fn a_first_sight_records_without_following() {
        let side = herdr::Side::Native;
        let now = Instant::now();
        let caches = one_focused(&side, "t1", now);
        let mut sync = HerdrViewSync::default();
        assert_eq!(sync.next(always(Some((1, None, false)), &caches)), None);
    }

    #[test]
    fn a_native_session_follows_a_change_on_any_side() {
        let side = herdr::Side::Wsl("ubuntu".into());
        let start = Instant::now();
        let first = one_focused(&side, "t1", start);
        let mut sync = HerdrViewSync::default();
        assert_eq!(sync.next(always(Some((1, None, false)), &first)), None);
        let later = start + Duration::from_millis(1);
        let second = one_focused(&side, "t2", later);
        assert_eq!(
            sync.next(always(Some((1, None, false)), &second)),
            Some(HerdrViewAction::Follow(HerdrKey {
                side: side.clone(),
                terminal_id: "t2".into(),
            }))
        );
    }

    /// The default mode is what ships, and it must stay blind to a change
    /// made while a native session is active.
    #[test]
    fn the_default_mode_ignores_the_trail() {
        let side = herdr::Side::Native;
        let start = Instant::now();
        let first = one_focused(&side, "t1", start);
        let mut sync = HerdrViewSync::default();
        let mut inputs = always(Some((1, None, false)), &first);
        inputs.follow = FollowFocus::Herdr;
        assert_eq!(sync.next(inputs), None);
        let later = start + Duration::from_millis(1);
        let second = one_focused(&side, "t2", later);
        let mut inputs = always(Some((1, None, false)), &second);
        inputs.follow = FollowFocus::Herdr;
        assert_eq!(sync.next(inputs), None);
    }

    /// A change the user is already looking at is not somewhere to go.
    #[test]
    fn a_change_onto_the_active_pane_records_without_following() {
        // A direct attach, so the shared-view path filters this session out
        // and leaves the active-pane question to the trail.
        let side = herdr::Side::Wsl("ubuntu".into());
        let key = HerdrKey { side: side.clone(), terminal_id: "t2".into() };
        let start = Instant::now();
        let first = one_focused(&side, "t1", start);
        let mut sync = HerdrViewSync::default();
        let mut first_inputs = always(Some((1, Some(&key), true)), &first);
        first_inputs.attach = AttachMode::Agent;
        assert_eq!(sync.next(first_inputs), None);
        let later = start + Duration::from_millis(1);
        let second = one_focused(&side, "t2", later);
        let mut second_inputs = always(Some((1, Some(&key), true)), &second);
        second_inputs.attach = AttachMode::Agent;
        assert_eq!(sync.next(second_inputs), None);
        // The active-pane branch must have updated the entry, not merely
        // skipped the edge: a third sample back on the old pane is a real
        // change against the recorded "t2", so it is followed.
        let latest = later + Duration::from_millis(1);
        let third = one_focused(&side, "t1", latest);
        let mut third_inputs = always(Some((1, Some(&key), true)), &third);
        third_inputs.attach = AttachMode::Agent;
        assert_eq!(
            sync.next(third_inputs),
            Some(HerdrViewAction::Follow(HerdrKey {
                side: side.clone(),
                terminal_id: "t1".into()
            }))
        );
    }

    /// A listing already in flight when alacritree moved herdr's focus
    /// reports the old pane, and acting on it would run the change backwards.
    #[test]
    fn a_sample_older_than_the_stamp_forms_no_edge() {
        let side = herdr::Side::Native;
        let start = Instant::now();
        let mut sync = HerdrViewSync::default();
        let first = one_focused(&side, "t1", start);
        assert_eq!(sync.next(always(Some((1, None, false)), &first)), None);
        let moved = start + Duration::from_millis(5);
        sync.moved_focus(&HerdrKey { side: side.clone(), terminal_id: "t3".into() }, moved);
        let in_flight = one_focused(&side, "t2", start + Duration::from_millis(2));
        assert_eq!(sync.next(always(Some((1, None, false)), &in_flight)), None);
    }

    /// `attached` stamps the trail, so a pane it just opened is not mistaken
    /// for a change herdr made once the user has moved on to something else.
    #[test]
    fn attached_stamps_the_trail_so_its_own_pane_is_not_a_change() {
        let side = herdr::Side::Wsl("ubuntu".into());
        let start = Instant::now();
        let mut sync = HerdrViewSync::default();
        let first = one_focused(&side, "t1", start);
        assert_eq!(sync.next(always(Some((1, None, false)), &first)), None);

        let attached_at = start + Duration::from_millis(1);
        let key = HerdrKey { side: side.clone(), terminal_id: "t2".into() };
        sync.attached(2, Some(&key), attached_at);

        // The active session has moved on by the time the poll confirms
        // herdr is on the pane the attach just opened.
        let later = attached_at + Duration::from_millis(1);
        let second = one_focused(&side, "t2", later);
        assert_eq!(sync.next(always(Some((3, None, false)), &second)), None);
    }

    /// A side that stops answering empties its listing, and a side whose
    /// distro stopped loses its cache entirely; neither is a focus change.
    #[test]
    fn a_silent_or_vanished_side_forms_no_edge() {
        let side = herdr::Side::Wsl("ubuntu".into());
        let start = Instant::now();
        let first = one_focused(&side, "t1", start);
        let mut sync = HerdrViewSync::default();
        assert_eq!(sync.next(always(Some((1, None, false)), &first)), None);
        let later = start + Duration::from_millis(1);
        let silent = vec![herdr::EndpointCache::for_test(side.clone(), Vec::new(), later)];
        assert_eq!(sync.next(always(Some((1, None, false)), &silent)), None);
        let gone: Vec<herdr::EndpointCache> = Vec::new();
        assert_eq!(sync.next(always(Some((1, None, false)), &gone)), None);
        // The side comes back: its first reading is a baseline again, not the
        // change it looks like against the entry that used to be there.
        let back = one_focused(&side, "t9", later + Duration::from_millis(1));
        assert_eq!(sync.next(always(Some((1, None, false)), &back)), None);
    }

    /// Following acts on any reachable side, and a side nobody touched is
    /// not a change.
    #[test]
    fn a_change_on_one_side_leaves_a_quiet_side_alone() {
        let native = herdr::Side::Native;
        let wsl = herdr::Side::Wsl("ubuntu".into());
        let start = Instant::now();
        let mut sync = HerdrViewSync::default();
        let mut first = one_focused(&native, "n1", start);
        first.extend(one_focused(&wsl, "w1", start));
        assert_eq!(sync.next(always(Some((1, None, false)), &first)), None);

        let later = start + Duration::from_millis(1);
        let mut second = one_focused(&native, "n1", later);
        second.extend(one_focused(&wsl, "w2", later));
        assert_eq!(
            sync.next(always(Some((1, None, false)), &second)),
            Some(HerdrViewAction::Follow(HerdrKey { side: wsl.clone(), terminal_id: "w2".into() })),
            "the side that moved is the one to go to"
        );
    }

    /// The shared-view path owns a session that shows herdr's view; the trail
    /// speaks only when that path has nothing to say.
    #[test]
    fn the_shared_view_path_wins_over_the_trail() {
        let side = herdr::Side::Native;
        let key = HerdrKey { side: side.clone(), terminal_id: "t1".into() };
        let start = Instant::now();
        let caches = one_focused(&side, "t2", start);
        let mut sync = HerdrViewSync::default();
        assert_eq!(
            sync.next(always(Some((1, Some(&key), false)), &caches)),
            Some(HerdrViewAction::Focus(1))
        );
    }

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
        let active = Some((1, Some(&t1), false));
        let no_caches: Vec<herdr::EndpointCache> = Vec::new();
        assert_eq!(
            sync.next(inputs(active, AttachMode::Session, &no_caches, false)),
            Some(HerdrViewAction::Focus(1))
        );
        let focused_at = Instant::now();
        sync.settled(1, true, focused_at);
        let caches = vec![herdr::EndpointCache::for_test(
            side.clone(),
            panes.clone(),
            focused_at + Duration::from_millis(1),
        )];
        assert_eq!(
            sync.next(inputs(active, AttachMode::Session, &caches, false)),
            Some(HerdrViewAction::Follow(t2.clone()))
        );
        sync.attached(2, None, focused_at + Duration::from_millis(2));
        assert_eq!(
            sync.next(inputs(Some((2, Some(&t2), false)), AttachMode::Session, &caches, false)),
            None
        );
        assert_eq!(sync.next(inputs(None, AttachMode::Session, &caches, false)), None);
        assert_eq!(
            sync.next(inputs(active, AttachMode::Session, &caches, false)),
            Some(HerdrViewAction::Focus(1))
        );
    }

    #[test]
    fn herdr_shared_view_refocuses_after_an_ordinary_session() {
        let key = herdr::HerdrKey { side: herdr::Side::Native, terminal_id: "t1".into() };
        let mut sync = HerdrViewSync::default();
        sync.attached(1, None, Instant::now());
        let no_caches: Vec<herdr::EndpointCache> = Vec::new();
        assert_eq!(sync.next(inputs(None, AttachMode::Session, &no_caches, false)), None);
        assert_eq!(
            sync.next(inputs(Some((1, Some(&key), false)), AttachMode::Session, &no_caches, false)),
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
        sync.attached(1, None, focused_at);
        let active = Some((1, Some(&key), false));
        let caches = vec![herdr::EndpointCache::for_test(
            side.clone(),
            panes.clone(),
            focused_at + Duration::from_millis(1),
        )];
        assert!(matches!(
            sync.next(inputs(active, AttachMode::Session, &caches, false)),
            Some(HerdrViewAction::Follow(_))
        ));
        assert_eq!(sync.next(inputs(active, AttachMode::Session, &caches, false)), None);
        let caches = vec![herdr::EndpointCache::for_test(
            side.clone(),
            panes.clone(),
            focused_at + Duration::from_millis(2),
        )];
        assert!(matches!(
            sync.next(inputs(active, AttachMode::Session, &caches, false)),
            Some(HerdrViewAction::Follow(_))
        ));
        sync.settled(1, false, focused_at + Duration::from_millis(3));
        let caches = vec![herdr::EndpointCache::for_test(
            side.clone(),
            panes.clone(),
            focused_at + Duration::from_millis(4),
        )];
        assert_eq!(sync.next(inputs(active, AttachMode::Session, &caches, false)), None);
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
        let active = Some((1, Some(&key), false));
        let no_caches: Vec<herdr::EndpointCache> = Vec::new();
        let started = Instant::now();
        assert_eq!(
            sync.next(inputs(active, AttachMode::Agent, &no_caches, false)),
            Some(HerdrViewAction::Focus(1))
        );
        assert_eq!(sync.next(inputs(active, AttachMode::Agent, &no_caches, true)), None);
        let settled = started + Duration::from_millis(1);
        sync.settled(1, true, settled);
        let stale = vec![herdr::EndpointCache::for_test(side.clone(), panes.clone(), started)];
        assert_eq!(sync.next(inputs(active, AttachMode::Agent, &stale, false)), None);
        let fresh_at = settled + Duration::from_millis(1);
        let foreign =
            vec![herdr::EndpointCache::for_test(other_side.clone(), panes.clone(), fresh_at)];
        assert_eq!(sync.next(inputs(active, AttachMode::Agent, &foreign, false)), None);
        let fresh = vec![herdr::EndpointCache::for_test(side.clone(), panes.clone(), fresh_at)];
        assert_eq!(sync.next(inputs(active, AttachMode::Agent, &fresh, true)), None);
        assert_eq!(
            sync.next(inputs(Some((1, Some(&key), true)), AttachMode::Agent, &fresh, false)),
            None
        );
        assert_eq!(
            sync.next(inputs(active, AttachMode::Agent, &fresh, false)),
            Some(HerdrViewAction::Focus(1))
        );
    }

    #[test]
    fn herdr_focus_completion_cannot_restore_a_view_left_while_pending() {
        let key = herdr::HerdrKey { side: herdr::Side::Native, terminal_id: "t1".into() };
        let active = Some((1, Some(&key), false));
        let mut sync = HerdrViewSync::default();
        let no_caches: Vec<herdr::EndpointCache> = Vec::new();
        assert_eq!(
            sync.next(inputs(active, AttachMode::Session, &no_caches, false)),
            Some(HerdrViewAction::Focus(1))
        );
        assert_eq!(sync.next(inputs(None, AttachMode::Session, &no_caches, true)), None);
        sync.settled(1, true, Instant::now());
        assert_eq!(
            sync.next(inputs(active, AttachMode::Session, &no_caches, false)),
            Some(HerdrViewAction::Focus(1))
        );
        sync.settled(1, false, Instant::now());
        assert_eq!(sync.next(inputs(active, AttachMode::Session, &no_caches, false)), None);
    }

    /// The setting governs whether herdr may move alacritree.  A shared view
    /// still owes herdr a focus call, or it draws the wrong pane.
    #[test]
    fn off_still_asks_herdr_for_the_shared_view_pane() {
        let key = herdr::HerdrKey { side: herdr::Side::Native, terminal_id: "t1".into() };
        let caches: Vec<herdr::EndpointCache> = Vec::new();
        let mut sync = HerdrViewSync::default();
        let mut off = inputs(Some((1, Some(&key), false)), AttachMode::Session, &caches, false);
        off.follow = FollowFocus::Off;
        assert_eq!(sync.next(off), Some(HerdrViewAction::Focus(1)));
    }

    /// The shared-view path returns its follow on the same call, in every
    /// mode that follows.
    #[test]
    fn a_shared_view_follows_herdr_in_every_mode() {
        for mode in [FollowFocus::Herdr, FollowFocus::Always] {
            let side = herdr::Side::Native;
            let t1 = HerdrKey { side: side.clone(), terminal_id: "t1".into() };
            let t2 = HerdrKey { side: side.clone(), terminal_id: "t2".into() };
            let panes = herdr::Listing::Panes.parse(
                r#"{"result":{"panes":[
                    {"terminal_id":"t2","pane_id":"w2:p1","tab_id":"w2:t1","focused":true}
                ]}}"#,
            );
            let mut sync = HerdrViewSync::default();
            let focused_at = Instant::now();
            sync.attached(1, None, focused_at);
            let caches = vec![herdr::EndpointCache::for_test(
                side.clone(),
                panes.clone(),
                focused_at + Duration::from_millis(1),
            )];
            let mut at = inputs(Some((1, Some(&t1), false)), AttachMode::Session, &caches, false);
            at.follow = mode;
            assert_eq!(
                sync.next(at),
                Some(HerdrViewAction::Follow(t2.clone())),
                "mode {mode:?} delayed or dropped a shared-view follow"
            );
        }
    }

    /// `Off` suppresses the follow the shared-view path would otherwise
    /// return, rather than deferring it to whenever the mode next changes.
    #[test]
    fn off_suppresses_a_shared_view_follow() {
        let side = herdr::Side::Native;
        let t1 = HerdrKey { side: side.clone(), terminal_id: "t1".into() };
        let panes = herdr::Listing::Panes.parse(
            r#"{"result":{"panes":[
                {"terminal_id":"t2","pane_id":"w2:p1","tab_id":"w2:t1","focused":true}
            ]}}"#,
        );
        let mut sync = HerdrViewSync::default();
        let focused_at = Instant::now();
        sync.attached(1, None, focused_at);
        let caches = vec![herdr::EndpointCache::for_test(
            side.clone(),
            panes.clone(),
            focused_at + Duration::from_millis(1),
        )];
        let mut off = inputs(Some((1, Some(&t1), false)), AttachMode::Session, &caches, false);
        off.follow = FollowFocus::Off;
        assert_eq!(sync.next(off), None);
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

    /// A pane with no agent in it has no direct attach on any side, so its
    /// session is a shared view and keeps asking for its own pane.
    #[test]
    fn an_agentless_pane_asks_herdr_for_its_pane_on_every_side() {
        let key = herdr::HerdrKey { side: herdr::Side::Wsl("d".into()), terminal_id: "t1".into() };
        assert!(needs_view_focus(Some(&key), AttachMode::Agent, false, 1, None));
    }
}
