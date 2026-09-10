//! Which herdr pane alacritree is showing, and when to tell herdr to move.
//!
//! Every herdr client draws the one pane herdr has focused, so a session
//! sharing herdr's view has to ask for its own pane before its client can
//! start, and follows herdr afterwards.

use std::collections::HashMap;
use std::time::{Duration, Instant};

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

/// How long the user has to stop typing before a follow lands.  Long enough
/// to clear the pause between keystrokes and short enough that a deliberate
/// pause is not mistaken for continued work.
const FOLLOW_QUIET_GAP: Duration = Duration::from_millis(750);

/// How long a proposed follow keeps waiting.  Matches `PROBE_GRACE`, the
/// codebase's one existing answer to "the user is active".  Past it the
/// change is stale, and moving the user then is worse than not moving them.
const FOLLOW_EXPIRY: Duration = Duration::from_secs(10);

/// A follow that has been proposed and is waiting for the user to stop
/// typing.  Both clocks count attentive time only: counting wall-clock time
/// would expire a follow while the user was in another window, which is the
/// catch-up-on-return case the trail exists to preserve.
struct PendingFollow {
    key: HerdrKey,
    /// The session that was active when this was proposed.  A different one
    /// means the proposal no longer describes the situation.
    active: Option<SessionId>,
    /// Attentive time since the last direct input.
    quiet: Duration,
    /// Attentive time since the proposal.
    age: Duration,
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
    pending: Option<PendingFollow>,
    /// When `next` last counted attentive time, so a frame's contribution is
    /// the gap since the previous one rather than a fixed tick.
    ticked_at: Option<Instant>,
    /// The direct-input reading the current `quiet` was measured from.
    input_seen: Option<Instant>,
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
    /// This frame's clock reading, for timing a pending follow's quiet gap
    /// and expiry.
    pub now: Instant,
    /// When the user last gave the app a direct-input event, if ever.  A
    /// pending follow's quiet gap is measured from this rather than from
    /// `now`, so it restarts on new input instead of aging out from under
    /// continued typing.
    pub last_direct_input: Option<Instant>,
}

impl HerdrViewSync {
    pub fn closed(&mut self, id: SessionId, key: Option<&HerdrKey>, at: Instant) {
        if self.visible == Some(id) {
            self.visible = None;
            self.follow_after = None;
        }
        if self.focused == Some(id) {
            self.focused = None;
        }
        // A follow to a row the user just closed would respawn its attach
        // client.  herdr still reports that pane as focused, so only stamping
        // the refusal stops the same edge re-forming on the next frame.
        if let Some(key) = key.filter(|key| self.pending.as_ref().is_some_and(|p| &p.key == *key)) {
            self.moved_focus(key, at);
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
    /// for is never mistaken for one the user made inside herdr, and a
    /// pending follow on the same side — proposed against a state this
    /// stamp just superseded — goes with it.
    pub fn moved_focus(&mut self, key: &HerdrKey, at: Instant) {
        self.trail.insert(key.side.clone(), TrailEntry {
            terminal_id: key.terminal_id.clone(),
            stamped_at: at,
        });
        if self.pending.as_ref().is_some_and(|pending| pending.key.side == key.side) {
            self.pending = None;
        }
    }

    /// The first side whose focused pane differs from what the trail holds.
    /// Two sides changing between one frame and the next is rare enough that
    /// a deliberate tiebreak would be inventing a rule nobody can observe,
    /// and the other side's change is still a change on the next frame.
    fn trail_edge(&mut self, inputs: &ViewInputs<'_>) -> Option<HerdrKey> {
        // A pending targeting a side that just vanished would follow to an
        // unreachable pane; its trail entry is gone the same way.
        let live = |side: &Side| inputs.caches.iter().any(|cache| cache.side() == side);
        self.trail.retain(|side, _| live(side));
        if self.pending.as_ref().is_some_and(|pending| !live(&pending.key.side)) {
            self.pending = None;
        }
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
            // An id appearing where there was no entry is first sight.
            let Some(entry) = self.trail.get(cache.side()) else {
                self.moved_focus(&key, sampled_at);
                continue;
            };
            if sampled_at <= entry.stamped_at {
                continue;
            }
            let trailed = entry.terminal_id == focused.terminal_id;
            self.void_pending(&key);
            if trailed {
                continue;
            }
            if active_key == Some(&key) {
                self.moved_focus(&key, sampled_at);
                continue;
            }
            edge.get_or_insert(key);
        }
        edge
    }

    /// Drop a pending whose side herdr has moved off the proposed pane.  The
    /// proposal exists because herdr focused that pane; following once herdr
    /// is elsewhere lands the user on a pane herdr has left, which a shared
    /// view there would then drag herdr back to.
    fn void_pending(&mut self, focused: &HerdrKey) {
        let stale = self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.key.side == focused.side && pending.key != *focused);
        if stale {
            self.pending = None;
        }
    }

    pub fn next(&mut self, inputs: ViewInputs<'_>) -> Option<HerdrViewAction> {
        let elapsed = self.tick(&inputs);
        if let Some(action) = self.shared_view(&inputs) {
            // The shared-view path outranks the trail.  The trail itself is
            // untouched, so the tick baseline goes with the pending, or a
            // reborn one inherits this frame's gap.
            if self.pending.take().is_some() {
                self.ticked_at = None;
            }
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
        // The trail serves `always` alone, and the mode is read once at
        // startup, so nothing recorded here in another mode could ever be
        // wanted later.  A user who did not opt in pays for none of it.
        if inputs.follow != FollowFocus::Always {
            return None;
        }
        if let Some(edge) = self.trail_edge(&inputs) {
            self.propose(edge, &inputs);
        }
        self.deliver(&inputs, elapsed)
    }

    /// Attentive time since the previous frame.  The baseline is replaced on
    /// every call, including the busy and inattentive frames that return
    /// before `deliver`, so the stretch they cover is charged to a frame that
    /// discards it rather than to the pending follow afterwards.
    fn tick(&mut self, inputs: &ViewInputs<'_>) -> Duration {
        let previous = self.ticked_at.replace(inputs.now);
        previous.map_or(Duration::ZERO, |previous| inputs.now.saturating_duration_since(previous))
    }

    /// A second edge replaces the target and restarts the clock; re-seeing
    /// the same edge, which happens every frame until the trail is stamped,
    /// changes nothing.  A move back to the trailed pane forms no edge to see,
    /// so `void_pending` is what retires the proposal there.  `input_seen` is
    /// left for `deliver` to reconcile, so a pending born this frame is never
    /// credited with quiet time that predates its own proposal.
    fn propose(&mut self, key: HerdrKey, inputs: &ViewInputs<'_>) {
        if self.pending.as_ref().is_some_and(|pending| pending.key == key) {
            return;
        }
        self.pending = Some(PendingFollow {
            key,
            active: inputs.active.map(|(id, ..)| id),
            quiet: Duration::ZERO,
            age: Duration::ZERO,
        });
    }

    fn deliver(&mut self, inputs: &ViewInputs<'_>, elapsed: Duration) -> Option<HerdrViewAction> {
        let active = inputs.active.map(|(id, ..)| id);
        // The proposal was made against a situation that no longer holds.
        if self.pending.as_ref().is_some_and(|pending| pending.active != active) {
            self.pending = None;
            return None;
        }
        // Owns `input_seen`: a pending `propose` just installed has never
        // been compared against it, so this frame reconciles the two.
        let input_moved = self.input_seen != inputs.last_direct_input;
        if input_moved {
            self.input_seen = inputs.last_direct_input;
        }
        let (expired, ready) = {
            let pending = self.pending.as_mut()?;
            pending.quiet = if input_moved { Duration::ZERO } else { pending.quiet + elapsed };
            pending.age += elapsed;
            (pending.age >= FOLLOW_EXPIRY, pending.quiet >= FOLLOW_QUIET_GAP)
        };
        if expired {
            let key = self.pending.take()?.key;
            // Recording the decision not to go, or the same stale change
            // would be re-proposed on every frame that follows.
            self.moved_focus(&key, inputs.now);
            return None;
        }
        if !ready {
            return None;
        }
        let key = self.pending.take()?.key;
        // The target may have become the active pane while this waited.
        inputs
            .active
            .and_then(|(_, active, _)| active)
            .is_none_or(|active| active != &key)
            .then_some(HerdrViewAction::Follow(key))
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
        ViewInputs {
            active,
            attach,
            follow: FollowFocus::Herdr,
            caches,
            attentive: true,
            busy,
            now: Instant::now(),
            last_direct_input: None,
        }
    }

    fn always<'a>(
        active: Option<(SessionId, Option<&'a HerdrKey>, bool)>,
        caches: &'a [herdr::EndpointCache],
        now: Instant,
    ) -> ViewInputs<'a> {
        ViewInputs {
            active,
            attach: AttachMode::Session,
            follow: FollowFocus::Always,
            caches,
            attentive: true,
            busy: false,
            now,
            last_direct_input: None,
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
        assert_eq!(sync.next(always(Some((1, None, false)), &caches, now)), None);
    }

    #[test]
    fn a_native_session_follows_a_change_on_any_side() {
        let side = herdr::Side::Wsl("ubuntu".into());
        let start = Instant::now();
        let first = one_focused(&side, "t1", start);
        let mut sync = HerdrViewSync::default();
        assert_eq!(sync.next(always(Some((1, None, false)), &first, start)), None);
        let later = start + Duration::from_millis(1);
        let second = one_focused(&side, "t2", later);
        assert_eq!(sync.next(always(Some((1, None, false)), &second, later)), None);
        let quiet = later + FOLLOW_QUIET_GAP + Duration::from_millis(1);
        assert_eq!(
            sync.next(always(Some((1, None, false)), &second, quiet)),
            Some(HerdrViewAction::Follow(HerdrKey {
                side: side.clone(),
                terminal_id: "t2".into(),
            }))
        );
    }

    /// The modes that ship must stay blind to a change made while a native
    /// session is active, right through the gap where `always` would follow.
    #[test]
    fn the_modes_that_are_not_always_ignore_the_trail() {
        for mode in [FollowFocus::Herdr, FollowFocus::Off] {
            let side = herdr::Side::Native;
            let start = Instant::now();
            let first = one_focused(&side, "t1", start);
            let mut sync = HerdrViewSync::default();
            let mut inputs = always(Some((1, None, false)), &first, start);
            inputs.follow = mode;
            assert_eq!(sync.next(inputs), None);

            let later = start + Duration::from_millis(1);
            let second = one_focused(&side, "t2", later);
            let mut inputs = always(Some((1, None, false)), &second, later);
            inputs.follow = mode;
            assert_eq!(sync.next(inputs), None);

            let quiet = later + FOLLOW_QUIET_GAP + Duration::from_millis(1);
            let mut inputs = always(Some((1, None, false)), &second, quiet);
            inputs.follow = mode;
            assert_eq!(sync.next(inputs), None, "mode {mode:?} followed a change from the trail");
        }
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
        let mut first_inputs = always(Some((1, Some(&key), true)), &first, start);
        first_inputs.attach = AttachMode::Agent;
        assert_eq!(sync.next(first_inputs), None);
        let later = start + Duration::from_millis(1);
        let second = one_focused(&side, "t2", later);
        let mut second_inputs = always(Some((1, Some(&key), true)), &second, later);
        second_inputs.attach = AttachMode::Agent;
        assert_eq!(sync.next(second_inputs), None);
        // The active-pane branch must have updated the entry, not merely
        // skipped the edge: a third sample back on the old pane is a real
        // change against the recorded "t2", so it is followed.
        let latest = later + Duration::from_millis(1);
        let third = one_focused(&side, "t1", latest);
        let mut third_inputs = always(Some((1, Some(&key), true)), &third, latest);
        third_inputs.attach = AttachMode::Agent;
        assert_eq!(sync.next(third_inputs), None);
        let quiet = latest + FOLLOW_QUIET_GAP + Duration::from_millis(1);
        let mut quiet_inputs = always(Some((1, Some(&key), true)), &third, quiet);
        quiet_inputs.attach = AttachMode::Agent;
        assert_eq!(
            sync.next(quiet_inputs),
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
        assert_eq!(sync.next(always(Some((1, None, false)), &first, start)), None);
        let moved = start + Duration::from_millis(5);
        sync.moved_focus(&HerdrKey { side: side.clone(), terminal_id: "t3".into() }, moved);
        let in_flight = one_focused(&side, "t2", start + Duration::from_millis(2));
        assert_eq!(
            sync.next(always(Some((1, None, false)), &in_flight, start + Duration::from_millis(2))),
            None
        );
    }

    /// `attached` stamps the trail, so a pane it just opened is not mistaken
    /// for a change herdr made once the user has moved on to something else.
    #[test]
    fn attached_stamps_the_trail_so_its_own_pane_is_not_a_change() {
        let side = herdr::Side::Wsl("ubuntu".into());
        let start = Instant::now();
        let mut sync = HerdrViewSync::default();
        let first = one_focused(&side, "t1", start);
        assert_eq!(sync.next(always(Some((1, None, false)), &first, start)), None);

        let attached_at = start + Duration::from_millis(1);
        let key = HerdrKey { side: side.clone(), terminal_id: "t2".into() };
        sync.attached(2, Some(&key), attached_at);

        // The active session has moved on by the time the poll confirms
        // herdr is on the pane the attach just opened.
        let later = attached_at + Duration::from_millis(1);
        let second = one_focused(&side, "t2", later);
        assert_eq!(sync.next(always(Some((3, None, false)), &second, later)), None);
    }

    /// A side that stops answering empties its listing, and a side whose
    /// distro stopped loses its cache entirely; neither is a focus change.
    #[test]
    fn a_silent_or_vanished_side_forms_no_edge() {
        let side = herdr::Side::Wsl("ubuntu".into());
        let start = Instant::now();
        let first = one_focused(&side, "t1", start);
        let mut sync = HerdrViewSync::default();
        assert_eq!(sync.next(always(Some((1, None, false)), &first, start)), None);
        let later = start + Duration::from_millis(1);
        let silent = vec![herdr::EndpointCache::for_test(side.clone(), Vec::new(), later)];
        assert_eq!(sync.next(always(Some((1, None, false)), &silent, later)), None);
        let gone: Vec<herdr::EndpointCache> = Vec::new();
        assert_eq!(sync.next(always(Some((1, None, false)), &gone, later)), None);
        // The side comes back: its first reading is a baseline again, not the
        // change it looks like against the entry that used to be there.
        let back = one_focused(&side, "t9", later + Duration::from_millis(1));
        assert_eq!(
            sync.next(always(Some((1, None, false)), &back, later + Duration::from_millis(1))),
            None
        );
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
        assert_eq!(sync.next(always(Some((1, None, false)), &first, start)), None);

        let later = start + Duration::from_millis(1);
        let mut second = one_focused(&native, "n1", later);
        second.extend(one_focused(&wsl, "w2", later));
        assert_eq!(sync.next(always(Some((1, None, false)), &second, later)), None);
        let quiet = later + FOLLOW_QUIET_GAP + Duration::from_millis(1);
        assert_eq!(
            sync.next(always(Some((1, None, false)), &second, quiet)),
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
            sync.next(always(Some((1, Some(&key), false)), &caches, start)),
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

    /// A pane a script created can arrive mid-command, and following moves
    /// the keyboard, so it waits for the typing to stop.
    #[test]
    fn a_follow_waits_for_a_gap_in_typing() {
        let side = herdr::Side::Native;
        let start = Instant::now();
        let mut sync = HerdrViewSync::default();
        let first = one_focused(&side, "t1", start);
        assert_eq!(sync.next(always(Some((1, None, false)), &first, start)), None);

        let sampled = start + Duration::from_millis(1);
        let second = one_focused(&side, "t2", sampled);
        let mut typing = always(Some((1, None, false)), &second, sampled);
        typing.last_direct_input = Some(sampled);
        assert_eq!(sync.next(typing), None, "proposed, not delivered");

        // Still typing half a second later.
        let mut typing =
            always(Some((1, None, false)), &second, sampled + Duration::from_millis(500));
        typing.last_direct_input = Some(sampled + Duration::from_millis(500));
        assert_eq!(sync.next(typing), None);

        // The gap arrives.
        let mut quiet =
            always(Some((1, None, false)), &second, sampled + Duration::from_millis(1300));
        quiet.last_direct_input = Some(sampled + Duration::from_millis(500));
        assert_eq!(
            sync.next(quiet),
            Some(HerdrViewAction::Follow(HerdrKey {
                side: side.clone(),
                terminal_id: "t2".into(),
            }))
        );
    }

    /// Moving the user long after the change is worse than not moving them.
    #[test]
    fn a_follow_expires_if_the_gap_never_comes() {
        let side = herdr::Side::Native;
        let start = Instant::now();
        let mut sync = HerdrViewSync::default();
        let first = one_focused(&side, "t1", start);
        assert_eq!(sync.next(always(Some((1, None, false)), &first, start)), None);
        let sampled = start + Duration::from_millis(1);
        let second = one_focused(&side, "t2", sampled);
        let mut at = sampled;
        let mut last_typed = at;
        // Typing without pause, in 200 ms frames, past the expiry.
        for _ in 0..60 {
            let mut typing = always(Some((1, None, false)), &second, at);
            typing.last_direct_input = Some(at);
            assert_eq!(sync.next(typing), None);
            last_typed = at;
            at += Duration::from_millis(200);
        }
        // The typing stops, and the change is stale rather than pending.
        // `last_direct_input` stays at the last keystroke: advancing it with
        // `now` would return `None` from the reset alone, proving nothing.
        let mut quiet = always(Some((1, None, false)), &second, at + Duration::from_secs(2));
        quiet.last_direct_input = Some(last_typed);
        assert_eq!(sync.next(quiet), None);
    }

    /// The pane herdr is on now is the only one worth going to.
    #[test]
    fn a_second_change_retargets_the_pending_follow_and_restarts_its_clock() {
        let side = herdr::Side::Native;
        let start = Instant::now();
        let mut sync = HerdrViewSync::default();
        let first = one_focused(&side, "t1", start);
        assert_eq!(sync.next(always(Some((1, None, false)), &first, start)), None);

        let a = start + Duration::from_millis(1);
        let second = one_focused(&side, "t2", a);
        let mut typing = always(Some((1, None, false)), &second, a);
        typing.last_direct_input = Some(a);
        assert_eq!(sync.next(typing), None);

        let b = a + Duration::from_millis(600);
        let third = one_focused(&side, "t3", b);
        let mut typing = always(Some((1, None, false)), &third, b);
        typing.last_direct_input = Some(b);
        assert_eq!(sync.next(typing), None);

        let mut quiet = always(Some((1, None, false)), &third, b + Duration::from_millis(800));
        quiet.last_direct_input = Some(b);
        assert_eq!(
            sync.next(quiet),
            Some(HerdrViewAction::Follow(HerdrKey {
                side: side.clone(),
                terminal_id: "t3".into(),
            })),
            "the newest change wins"
        );
    }

    /// The proposal was made against a situation that no longer holds.
    #[test]
    fn a_pending_follow_is_dropped_when_the_active_session_changes() {
        let side = herdr::Side::Native;
        let start = Instant::now();
        let mut sync = HerdrViewSync::default();
        let first = one_focused(&side, "t1", start);
        assert_eq!(sync.next(always(Some((1, None, false)), &first, start)), None);
        let sampled = start + Duration::from_millis(1);
        let second = one_focused(&side, "t2", sampled);
        let mut typing = always(Some((1, None, false)), &second, sampled);
        typing.last_direct_input = Some(sampled);
        assert_eq!(sync.next(typing), None);
        let later = sampled + Duration::from_millis(1300);
        let mut quiet = always(Some((7, None, false)), &second, later);
        quiet.last_direct_input = Some(sampled);
        assert_eq!(sync.next(quiet), None);
    }

    /// Following a row the user just closed would respawn its attach client,
    /// and herdr keeps reporting that row as focused for as long as it is.
    #[test]
    fn a_closed_target_suppresses_the_follow_rather_than_delaying_it() {
        let side = herdr::Side::Native;
        let target = HerdrKey { side: side.clone(), terminal_id: "t2".into() };
        let start = Instant::now();
        let mut sync = HerdrViewSync::default();
        let first = one_focused(&side, "t1", start);
        assert_eq!(sync.next(always(Some((1, None, false)), &first, start)), None);
        let sampled = start + Duration::from_millis(1);
        let second = one_focused(&side, "t2", sampled);
        let mut typing = always(Some((1, None, false)), &second, sampled);
        typing.last_direct_input = Some(sampled);
        assert_eq!(sync.next(typing), None);

        sync.closed(9, Some(&target), sampled + Duration::from_millis(1));

        // Two frames past the gap, because a drop the trail never learned
        // about only costs the re-formed edge the one frame it takes to
        // gather a gap of its own.
        for step in [1300, 2600] {
            let mut quiet =
                always(Some((1, None, false)), &second, sampled + Duration::from_millis(step));
            quiet.last_direct_input = Some(sampled);
            assert_eq!(sync.next(quiet), None, "the closed row was followed {step} ms in");
        }
    }

    /// A pending proposes going where herdr went.  herdr going back before
    /// the gap clears takes the reason with it, and following anyway lands
    /// the user on a pane herdr has left.
    #[test]
    fn a_pending_follow_is_dropped_when_herdr_returns_to_the_trailed_pane() {
        let side = herdr::Side::Native;
        let start = Instant::now();
        let mut sync = HerdrViewSync::default();
        let first = one_focused(&side, "t1", start);
        assert_eq!(sync.next(always(Some((1, None, false)), &first, start)), None);

        let moved_at = start + Duration::from_millis(1);
        let second = one_focused(&side, "t2", moved_at);
        assert_eq!(sync.next(always(Some((1, None, false)), &second, moved_at)), None);

        let back_at = moved_at + Duration::from_millis(1);
        let back = one_focused(&side, "t1", back_at);
        assert_eq!(sync.next(always(Some((1, None, false)), &back, back_at)), None);

        let quiet = back_at + FOLLOW_QUIET_GAP + Duration::from_millis(1);
        assert_eq!(sync.next(always(Some((1, None, false)), &back, quiet)), None);
    }

    /// The shared-view path outranks the trail, and a pending it leaves
    /// frozen behind it would otherwise deliver against a situation long
    /// gone by the time the session on screen returns to the trail.
    #[test]
    fn a_pending_follow_is_dropped_when_a_shared_view_takes_over() {
        let side = herdr::Side::Native;
        let start = Instant::now();
        let mut sync = HerdrViewSync::default();
        let first = one_focused(&side, "t1", start);
        assert_eq!(sync.next(always(Some((1, None, false)), &first, start)), None);
        let sampled = start + Duration::from_millis(1);
        let second = one_focused(&side, "t2", sampled);
        let mut typing = always(Some((1, None, false)), &second, sampled);
        typing.last_direct_input = Some(sampled);
        assert_eq!(sync.next(typing), None);

        // The active session switches to one the shared-view path owns.
        let shared_key = HerdrKey { side: side.clone(), terminal_id: "shared".into() };
        let shared_active = Some((9, Some(&shared_key), false));
        assert_eq!(
            sync.next(always(shared_active, &second, sampled + Duration::from_millis(2))),
            Some(HerdrViewAction::Focus(9))
        );

        // Long enough after the switch that a surviving pending would have
        // cleared its quiet gap.
        let mut later =
            always(Some((1, None, false)), &second, sampled + Duration::from_millis(802));
        later.last_direct_input = Some(sampled);
        assert_eq!(sync.next(later), None);
    }

    /// An attach on Windows can hold busy for seconds, and time the user
    /// never saw must not spend the follow's budget.
    #[test]
    fn a_busy_frame_neither_advances_nor_expires_the_pending_follow() {
        let side = herdr::Side::Native;
        let start = Instant::now();
        let mut sync = HerdrViewSync::default();
        let first = one_focused(&side, "t1", start);
        assert_eq!(sync.next(always(Some((1, None, false)), &first, start)), None);
        let sampled = start + Duration::from_millis(1);
        let second = one_focused(&side, "t2", sampled);
        let mut typing = always(Some((1, None, false)), &second, sampled);
        typing.last_direct_input = Some(sampled);
        assert_eq!(sync.next(typing), None);

        let mut busy = always(Some((1, None, false)), &second, sampled + Duration::from_secs(30));
        busy.last_direct_input = Some(sampled);
        busy.busy = true;
        assert_eq!(sync.next(busy), None);

        // The quiet gap is measured from the frames the user was present for,
        // so the follow survives the attach and lands after it.
        let mut quiet = always(
            Some((1, None, false)),
            &second,
            sampled + Duration::from_secs(30) + Duration::from_millis(800),
        );
        quiet.last_direct_input = Some(sampled);
        assert!(matches!(sync.next(quiet), Some(HerdrViewAction::Follow(_))));
    }

    /// Time spent in another window is the catch-up-on-return case the trail
    /// exists to preserve, not time the follow should age through.
    #[test]
    fn an_inattentive_frame_neither_advances_nor_expires_the_pending_follow() {
        let side = herdr::Side::Native;
        let start = Instant::now();
        let mut sync = HerdrViewSync::default();
        let first = one_focused(&side, "t1", start);
        assert_eq!(sync.next(always(Some((1, None, false)), &first, start)), None);
        let sampled = start + Duration::from_millis(1);
        let second = one_focused(&side, "t2", sampled);
        let mut typing = always(Some((1, None, false)), &second, sampled);
        typing.last_direct_input = Some(sampled);
        assert_eq!(sync.next(typing), None);

        let mut away = always(Some((1, None, false)), &second, sampled + Duration::from_secs(60));
        away.last_direct_input = Some(sampled);
        away.attentive = false;
        assert_eq!(sync.next(away), None);

        let mut back = always(
            Some((1, None, false)),
            &second,
            sampled + Duration::from_secs(60) + Duration::from_millis(800),
        );
        back.last_direct_input = Some(sampled);
        assert!(matches!(sync.next(back), Some(HerdrViewAction::Follow(_))));
    }

    /// Returning a follow stamps nothing; the caller stamps, on arrival or on
    /// refusal.  Until it does, the change stands and is proposed again.
    #[test]
    fn an_undelivered_follow_is_proposed_again() {
        let side = herdr::Side::Native;
        let start = Instant::now();
        let mut sync = HerdrViewSync::default();
        let first = one_focused(&side, "t1", start);
        assert_eq!(sync.next(always(Some((1, None, false)), &first, start)), None);
        let sampled = start + Duration::from_millis(1);
        let second = one_focused(&side, "t2", sampled);
        assert_eq!(sync.next(always(Some((1, None, false)), &second, sampled)), None);
        let quiet = sampled + Duration::from_millis(800);
        assert!(matches!(
            sync.next(always(Some((1, None, false)), &second, quiet)),
            Some(HerdrViewAction::Follow(_))
        ));
        // The app never called `attached`, so nothing stamped the trail.
        let again = quiet + Duration::from_millis(800);
        assert!(matches!(
            sync.next(always(Some((1, None, false)), &second, again)),
            Some(HerdrViewAction::Follow(_))
        ));
    }

    /// Giving up is a decision, and without recording it the same stale
    /// change would be re-proposed forever.
    #[test]
    fn an_expired_follow_stamps_the_trail() {
        let side = herdr::Side::Native;
        let start = Instant::now();
        let mut sync = HerdrViewSync::default();
        let first = one_focused(&side, "t1", start);
        assert_eq!(sync.next(always(Some((1, None, false)), &first, start)), None);
        let sampled = start + Duration::from_millis(1);
        let second = one_focused(&side, "t2", sampled);
        let mut at = sampled;
        for _ in 0..60 {
            let mut typing = always(Some((1, None, false)), &second, at);
            typing.last_direct_input = Some(at);
            assert_eq!(sync.next(typing), None);
            at += Duration::from_millis(200);
        }
        for _ in 0..10 {
            at += Duration::from_secs(1);
            assert_eq!(sync.next(always(Some((1, None, false)), &second, at)), None);
        }
    }
}
