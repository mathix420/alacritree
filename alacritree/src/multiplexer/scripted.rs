//! A multiplexer whose answers a test writes down in advance.
//!
//! [`Multiplexer`](super::Multiplexer) is a closed enum, so without a variant
//! here a fake satisfying [`MultiplexerSession`] could not become one, and app
//! tests reached past the seam into herdr's wire format instead.
//!
//! Nothing here runs a subprocess or watches a clock.  The listing is whatever
//! `set_panes` was handed, and each queued attach or create takes the next
//! answer the test pushed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use serde_json::{Value, json};

use super::model::{
    AttachAnswer, AttachRequest, CreateAnswer, CreateRequest, HarnessMark, ListedPane, Managed,
    StateTone, ViewState, ViewStep,
};
use super::{
    CreatedPane, Launch, MultiplexerKind, MultiplexerSession, Pane, PaneKey, PaneStatus,
    PaneTarget, Side,
};
use crate::config::{BakedGlyph, DEFAULT_SESSION_ICON, IconStyle};
use crate::session::SessionId;

/// Borrowed rather than drawn: every `BakedGlyph` is declared through
/// `baked_glyphs!` so the baked font subset covers it, and a scripted row
/// wants a neutral mark rather than one of its own.
const SCRIPTED_ICON: BakedGlyph = DEFAULT_SESSION_ICON;

/// An attach this adapter was asked for and has not answered yet.
pub(crate) struct QueuedAttach {
    pub key: PaneKey,
    pub target: PaneTarget,
    pub request: AttachRequest,
}

/// A create this adapter was asked for and has not answered yet.
pub(crate) struct QueuedCreate {
    pub side: Side,
    pub cwd: Option<String>,
    pub request: CreateRequest,
}

#[derive(Default)]
pub(crate) struct Scripted {
    enabled: bool,
    /// Whether opening a row attaches to the one pane or shares the whole
    /// view.  Both multiplexers in production answer this differently, so the
    /// app is tested against each.
    direct: bool,
    /// Panes matching no workspace are listed under Home.
    show_unmatched: bool,
    icon: IconStyle,
    generation: u64,
    sides: Vec<(Side, Vec<Pane>)>,
    /// Panes a session still holds after the listing stopped carrying them,
    /// which `retained` reports as no longer current.
    dropped: Vec<(Side, Pane)>,
    /// When a side last reported a terminal gone, keyed as `retained` is.
    gone: HashMap<(String, String), Instant>,
    /// The side a create naming none lands on.  `None` refuses, naming why.
    default_side: Option<Side>,
    attach_queue: Vec<QueuedAttach>,
    attach_answers: Vec<Result<Launch, String>>,
    create_queue: Vec<QueuedCreate>,
    create_answers: Vec<Result<CreatedPane, String>>,
    /// A move the user made inside the multiplexer, handed back by the next
    /// `sync_view` and then forgotten.
    follow: Option<PaneKey>,
    refused: Vec<PaneKey>,
    /// Sessions the app reported closed, in the order it reported them.
    closed: Vec<(SessionId, Option<PaneKey>)>,
    attached: Vec<(SessionId, PaneKey)>,
}

impl Scripted {
    pub(crate) fn key(side: &Side, terminal_id: &str) -> PaneKey {
        PaneKey {
            multiplexer: MultiplexerKind::Scripted,
            side: side.clone(),
            terminal_id: terminal_id.to_string(),
        }
    }

    /// A pane carrying only an identity, for the tests that care about nothing
    /// else.  `with_*` fills in what a particular test does care about.
    pub(crate) fn pane(terminal_id: &str) -> Pane {
        Pane {
            terminal_id: terminal_id.to_string(),
            pane_id: format!("p:{terminal_id}"),
            tab_id: Some(format!("t:{terminal_id}")),
            kind: None,
            title: None,
            status: None,
            focused: false,
            cwd: None,
            foreground_cwd: None,
        }
    }

    /// A pane on `side` in the listing, by terminal id.
    pub(crate) fn pane_mut(&mut self, side: &Side, terminal_id: &str) -> Option<&mut Pane> {
        self.sides
            .iter_mut()
            .find(|(s, _)| s == side)
            .and_then(|(_, panes)| panes.iter_mut().find(|p| p.terminal_id == terminal_id))
    }

    pub(crate) fn enable(&mut self) -> &mut Self {
        self.enabled = true;
        self
    }

    pub(crate) fn disable(&mut self) -> &mut Self {
        self.enabled = false;
        self
    }

    /// Open a row's pane on its own rather than by sharing the whole view.
    pub(crate) fn attach_directly(&mut self, direct: bool) -> &mut Self {
        self.direct = direct;
        self
    }

    pub(crate) fn show_unmatched(&mut self, show: bool) -> &mut Self {
        self.show_unmatched = show;
        self
    }

    /// The glyph a row draws for this multiplexer.
    pub(crate) fn set_icon(&mut self, icon: IconStyle) -> &mut Self {
        self.icon = icon;
        self
    }

    /// Replace what `side` is listing.  Every pane the side was carrying and
    /// this listing does not is remembered as dropped, so `retained` keeps
    /// describing a pane a session still holds.
    pub(crate) fn set_panes(&mut self, side: &Side, panes: Vec<Pane>) -> &mut Self {
        if let Some(at) = self.sides.iter().position(|(s, _)| s == side) {
            let (_, previous) = self.sides.remove(at);
            let now = Instant::now();
            for pane in previous {
                if panes.iter().any(|kept| kept.terminal_id == pane.terminal_id) {
                    continue;
                }
                self.gone.insert((side.name(), pane.terminal_id.clone()), now);
                self.dropped.push((side.clone(), pane));
            }
        }
        self.dropped.retain(|(s, pane)| {
            s != side || !panes.iter().any(|kept| kept.terminal_id == pane.terminal_id)
        });
        self.sides.push((side.clone(), panes));
        self.generation = self.generation.wrapping_add(1);
        self
    }

    /// The side a create that named none happens on.  Unset, a create with no
    /// side is refused the way a multiplexer with several servers refuses one.
    pub(crate) fn default_side(&mut self, side: Option<Side>) -> &mut Self {
        self.default_side = side;
        self
    }

    /// What the next queued attach resolves to.  Answers are taken in the
    /// order they were pushed.
    pub(crate) fn answer_attach(&mut self, launch: Result<Launch, String>) -> &mut Self {
        self.attach_answers.push(launch);
        self
    }

    pub(crate) fn answer_create(&mut self, pane: Result<CreatedPane, String>) -> &mut Self {
        self.create_answers.push(pane);
        self
    }

    /// Report a move the user made inside the multiplexer, for the app to
    /// follow on the next `sync_view`.
    pub(crate) fn propose_follow(&mut self, key: PaneKey) -> &mut Self {
        self.follow = Some(key);
        self
    }

    pub(crate) fn pending_attach(&self) -> &[QueuedAttach] {
        &self.attach_queue
    }

    pub(crate) fn pending_create(&self) -> &[QueuedCreate] {
        &self.create_queue
    }

    /// The panes the app told this adapter it would not follow to.
    pub(crate) fn refused(&self) -> &[PaneKey] {
        &self.refused
    }

    pub(crate) fn closed(&self) -> &[(SessionId, Option<PaneKey>)] {
        &self.closed
    }

    pub(crate) fn attached(&self) -> &[(SessionId, PaneKey)] {
        &self.attached
    }

    fn side(&self, side: &Side) -> Option<&[Pane]> {
        self.sides.iter().find(|(s, _)| s == side).map(|(_, panes)| panes.as_slice())
    }
}

/// Spelling a pane for a test.  Every field a multiplexer reports is public,
/// so these only exist to keep a pane one expression at the call site.
impl Pane {
    pub(crate) fn with_agent(mut self, kind: &str, status: PaneStatus) -> Self {
        self.kind = Some(kind.to_string());
        self.status = Some(status);
        self
    }

    /// An agent the multiplexer found but could not classify, which is a
    /// different answer from finding none.
    pub(crate) fn with_unclassified_agent(mut self) -> Self {
        self.status = Some(PaneStatus::Unknown);
        self
    }

    pub(crate) fn with_title(mut self, title: &str) -> Self {
        self.title = Some(title.to_string());
        self
    }

    pub(crate) fn in_dir(mut self, cwd: &str) -> Self {
        self.cwd = Some(cwd.to_string());
        self
    }

    /// The directory the pane's foreground job is in, which outranks its own
    /// when the two disagree.
    pub(crate) fn in_foreground_dir(mut self, cwd: &str) -> Self {
        self.foreground_cwd = Some(cwd.to_string());
        self
    }

    /// The pane the multiplexer's own window is showing.
    pub(crate) fn with_focus(mut self) -> Self {
        self.focused = true;
        self
    }

    /// A pane reached through its tab rather than by its own id, which is how
    /// a multiplexer resolving through an agent registry reaches one with no
    /// agent in it.
    pub(crate) fn in_tab(mut self, tab_id: Option<&str>) -> Self {
        self.tab_id = tab_id.map(str::to_string);
        self
    }
}

impl MultiplexerSession for Scripted {
    fn enabled(&self) -> bool {
        self.enabled
    }

    fn icon(&self) -> (&IconStyle, BakedGlyph) {
        (&self.icon, SCRIPTED_ICON)
    }

    /// The listing is whatever `set_panes` wrote, so there is nothing to
    /// refresh and no clock to wait on.
    fn poll(&mut self, _attached: &dyn Fn(&Side) -> bool) {}

    fn generation(&self) -> u64 {
        self.generation
    }

    fn panes(&self) -> Vec<(&Side, &Pane)> {
        self.sides
            .iter()
            .flat_map(|(side, panes)| panes.iter().map(move |pane| (side, pane)))
            .collect()
    }

    fn pane_index(&self, side: &Side, terminal_id: &str) -> Option<usize> {
        self.panes().iter().position(|(s, pane)| *s == side && pane.terminal_id == terminal_id)
    }

    fn pane_count(&self) -> usize {
        self.sides.iter().map(|(_, panes)| panes.len()).sum()
    }

    fn find(&self, side: &Side, terminal_id: &str) -> Option<&Pane> {
        self.side(side)?.iter().find(|pane| pane.terminal_id == terminal_id)
    }

    fn retained(&self, side: &Side, terminal_id: &str) -> Option<(&Pane, bool)> {
        if let Some(pane) = self.find(side, terminal_id) {
            return Some((pane, true));
        }
        self.dropped
            .iter()
            .find(|(s, pane)| s == side && pane.terminal_id == terminal_id)
            .map(|(_, pane)| (pane, false))
    }

    fn listed(&self, claimed: &[PaneKey], workspaces: &[PathBuf]) -> Vec<ListedPane<'_>> {
        if !self.enabled {
            return Vec::new();
        }
        self.panes()
            .into_iter()
            .map(|(side, pane)| {
                (Self::key(side, &pane.terminal_id), pane.workspace(side, workspaces), pane)
            })
            .filter(|(key, workspace, _)| {
                !claimed.contains(key) && (workspace.is_some() || self.show_unmatched)
            })
            .map(|(key, workspace, pane)| ListedPane { workspace, key, pane })
            .collect()
    }

    fn default_side(&self) -> Result<Side, String> {
        self.default_side.clone().ok_or_else(|| {
            let sides: Vec<String> = self.sides.iter().map(|(side, _)| side.name()).collect();
            match sides.as_slice() {
                [] => "no scripted server is running; start one, or name a side".to_string(),
                several => format!(
                    "no scripted server is focused and {} are running one; name a side",
                    several.join(" and ")
                ),
            }
        })
    }

    fn gone_since(&self, side: &Side, terminal_id: &str, bound_at: Instant) -> Option<Instant> {
        self.gone.get(&(side.name(), terminal_id.to_string())).copied().filter(|at| *at > bound_at)
    }

    fn pane_json(&self, side: &Side, terminal_id: &str, pane: Option<&Pane>) -> Value {
        json!({
            "name": MultiplexerKind::Scripted.to_string(),
            "side": side.name(),
            "terminal_id": terminal_id,
            "pane_id": pane.map(|pane| pane.pane_id.clone()),
            "tab_id": pane.and_then(|pane| pane.tab_id.clone()),
        })
    }

    fn managed(&self, side: &Side, pane: Option<&Pane>) -> Managed {
        Managed {
            multiplexer: MultiplexerKind::Scripted,
            detach: None,
            shared_view: !self.direct,
            kind: pane.and_then(|pane| pane.kind.clone()),
            title: pane.and_then(|pane| pane.title.clone()),
            mark: pane.and_then(|pane| pane.status).map(|status| self.mark(side, status)),
        }
    }

    fn mark(&self, _side: &Side, status: PaneStatus) -> HarnessMark {
        let tone = match status {
            PaneStatus::Blocked => StateTone::Blocked,
            PaneStatus::Working => StateTone::Working,
            PaneStatus::Done => StateTone::Done,
            PaneStatus::Idle => StateTone::Idle,
            PaneStatus::Unknown => StateTone::Unclear,
        };
        HarnessMark { glyph: "*", tone, label: status.label() }
    }

    fn attaches_directly(&self, _side: &Side, _has_agent: bool) -> bool {
        self.direct
    }

    fn open_directly(&self, target: &PaneTarget) -> Option<Launch> {
        self.direct.then(|| Launch {
            program: "scripted".to_string(),
            argv: vec!["open".to_string(), target.pane_id.clone()],
        })
    }

    fn shared_view(&self, key: &PaneKey) -> Option<Launch> {
        (!self.direct).then(|| Launch {
            program: "scripted".to_string(),
            argv: vec!["view".to_string(), key.terminal_id.clone()],
        })
    }

    fn queue_attach(&mut self, key: PaneKey, target: PaneTarget, request: AttachRequest) {
        if let Some(pending) = self.attach_queue.iter_mut().find(|p| p.key == key) {
            pending.request.waiters.extend(request.waiters);
            if request.focus.takes() && !pending.request.focus.takes() {
                pending.request.focus = request.focus;
            }
            return;
        }
        self.attach_queue.push(QueuedAttach { key, target, request });
    }

    /// One at a time, in the order they were queued, each taking the next
    /// scripted answer.  A queue with no answer left waiting is still
    /// pending, which is how a test holds an attach open.
    fn poll_attach(&mut self) -> (Option<AttachAnswer>, bool) {
        if self.attach_queue.is_empty() {
            return (None, false);
        }
        if self.attach_answers.is_empty() {
            return (None, true);
        }
        let launch = self.attach_answers.remove(0);
        let QueuedAttach { key, request, .. } = self.attach_queue.remove(0);
        (Some(AttachAnswer { key, request, launch }), false)
    }

    fn queue_create(&mut self, side: Side, cwd: Option<String>, request: CreateRequest) {
        self.create_queue.push(QueuedCreate { side, cwd, request });
    }

    fn poll_create(&mut self) -> Option<CreateAnswer> {
        if self.create_queue.is_empty() || self.create_answers.is_empty() {
            return None;
        }
        let pane = self.create_answers.remove(0);
        let QueuedCreate { side, request, .. } = self.create_queue.remove(0);
        Some(CreateAnswer { side, request, pane })
    }

    /// Hands back whatever `propose_follow` last wrote, once.  Nothing here
    /// watches a clock, so a test decides when a move happened.
    fn sync_view(&mut self, _state: ViewState<'_>) -> ViewStep {
        let follow = self.follow.take();
        ViewStep { repaint: follow.is_some(), follow }
    }

    fn view_attached(&mut self, id: SessionId, key: &PaneKey) {
        self.attached.push((id, key.clone()));
    }

    fn view_refused(&mut self, key: &PaneKey) {
        self.refused.push(key.clone());
    }

    fn session_closed(&mut self, id: SessionId, key: Option<&PaneKey>) {
        self.closed.push((id, key.cloned()));
        self.attach_queue.retain(|pending| Some(&pending.key) != key);
    }
}
