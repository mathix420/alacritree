//! zellij as a multiplexer alacritree hosts panes from.
//!
//! zellij runs one server per session, and a side may hold several, so a
//! pane is named by its session as well as its number.  A client attaches to
//! a whole session, never to one pane, and each client keeps its own focus,
//! so every attach is a shared view that is pointed at its pane once, when
//! it opens.  zellij detects no agents, so its panes carry no status.

mod cli;
mod listing;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::Instant;

pub use cli::{SideListing, attach, create_pane, focus_pane, list_side};
pub use listing::{split_terminal_id, terminal_id};
use serde_json::{Value, json};

use crate::config::{BakedGlyph, DEFAULT_HERDR_ICON, IconStyle, ZellijConfig};
use crate::multiplexer::{
    AttachAnswer, AttachRequest, CreateAnswer, CreateRequest, CreatedPane, HarnessMark, Launch,
    ListedPane, Managed, MultiplexerKind, MultiplexerSession, Pane, PaneKey, PaneStatus,
    PaneTarget, Side, StateTone, ViewState, ViewStep,
};
use crate::session::SessionId;
use crate::{jobs, wsl};

/// A shared-view attach waiting on its focus call.  `job` is `None` until the
/// attach at the head of the queue starts it.
struct PendingAttach {
    job: Option<jobs::Job<Result<Launch, String>>>,
    key: PaneKey,
    request: AttachRequest,
}

struct PendingCreate {
    job: jobs::Job<Result<CreatedPane, String>>,
    side: Side,
    request: CreateRequest,
}

pub(crate) struct Zellij {
    config: ZellijConfig,
    /// What each side answered in the last poll that landed.  A side that
    /// did not answer is absent rather than empty.
    sides: Vec<SideListing>,
    listing: Option<jobs::Job<Vec<SideListing>>>,
    last_poll: Option<Instant>,
    pending_attach: Vec<PendingAttach>,
    pending_create: Vec<PendingCreate>,
}

impl Zellij {
    pub(crate) fn new(config: ZellijConfig) -> Self {
        Self {
            config,
            sides: Vec::new(),
            listing: None,
            last_poll: None,
            pending_attach: Vec::new(),
            pending_create: Vec::new(),
        }
    }

    fn program(&self, side: &Side) -> String {
        match side {
            Side::Native => self.config.path.clone(),
            Side::Wsl(_) => self.wsl_program(),
        }
    }

    /// The same inside every distro, found through its login shell unless
    /// configured.
    fn wsl_program(&self) -> String {
        self.config.wsl_path.clone().unwrap_or_else(|| "zellij".to_string())
    }

    fn side(&self, side: &Side) -> Option<&SideListing> {
        self.sides.iter().find(|listing| &listing.side == side)
    }

    fn key(side: &Side, terminal_id: &str) -> PaneKey {
        PaneKey {
            multiplexer: MultiplexerKind::Zellij,
            side: side.clone(),
            terminal_id: terminal_id.to_string(),
        }
    }

    /// The session a new pane on `side` opens in: the configured one, or the
    /// only one running there.
    fn create_session(&self, side: &Side) -> Result<String, String> {
        if let Some(session) = &self.config.session {
            return Ok(session.clone());
        }
        let running =
            self.side(side).map(|listing| listing.sessions.as_slice()).unwrap_or_default();
        match running {
            [session] => Ok(session.clone()),
            [] => Err(format!("no zellij session is running on {}; start one", side.name())),
            several => Err(format!(
                "{} zellij sessions are running on {} ({}); set [integrations.zellij] session",
                several.len(),
                side.name(),
                several.join(", ")
            )),
        }
    }

    fn adopt(&mut self, sides: Vec<SideListing>) {
        self.sides = sides;
    }

    #[cfg(test)]
    pub(crate) fn adopt_for_test(&mut self, sides: Vec<SideListing>) {
        self.config.enabled = true;
        self.adopt(sides);
    }
}

impl MultiplexerSession for Zellij {
    fn enabled(&self) -> bool {
        self.config.enabled
    }

    fn icon(&self) -> (&IconStyle, BakedGlyph) {
        (&self.config.icon, DEFAULT_HERDR_ICON)
    }

    fn poll(&mut self, _attached: &dyn Fn(&Side) -> bool) {
        if !self.config.enabled {
            return;
        }
        if let Some(job) = &self.listing {
            match job.poll() {
                Some(sides) => self.adopt(sides),
                None if job.failed() => {},
                None => return,
            }
            self.listing = None;
        }
        if self.last_poll.is_some_and(|at| at.elapsed() < self.config.poll_interval) {
            return;
        }
        self.last_poll = Some(Instant::now());
        let native = self.program(&Side::Native);
        let inside_wsl = self.wsl_program();
        self.listing = Some(jobs::pool().spawn(jobs::Priority::Background, move |blocking| {
            let distros = wsl::running_distros(blocking).unwrap_or_default();
            std::iter::once(Side::Native)
                .chain(distros.into_iter().map(Side::Wsl))
                .filter_map(|side| {
                    let program = if side == Side::Native { &native } else { &inside_wsl };
                    list_side(program, &side).ok()
                })
                .collect()
        }));
    }

    /// The listing's own content, hashed, which changes exactly when a row
    /// would draw differently.
    fn generation(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        for (side, pane) in self.panes() {
            (side, &pane.terminal_id, &pane.title, pane.focused, &pane.cwd).hash(&mut hasher);
        }
        hasher.finish()
    }

    fn panes(&self) -> Vec<(&Side, &Pane)> {
        self.sides
            .iter()
            .flat_map(|listing| listing.panes.iter().map(move |pane| (&listing.side, pane)))
            .collect()
    }

    fn pane_index(&self, side: &Side, terminal_id: &str) -> Option<usize> {
        self.panes().iter().position(|(s, pane)| *s == side && pane.terminal_id == terminal_id)
    }

    fn pane_count(&self) -> usize {
        self.sides.iter().map(|listing| listing.panes.len()).sum()
    }

    fn find(&self, side: &Side, terminal_id: &str) -> Option<&Pane> {
        self.side(side)?.panes.iter().find(|pane| pane.terminal_id == terminal_id)
    }

    /// Every pane a session holds is still in the listing it is drawn from,
    /// so what the listing says is always current.
    fn retained(&self, side: &Side, terminal_id: &str) -> Option<(&Pane, bool)> {
        self.find(side, terminal_id).map(|pane| (pane, true))
    }

    fn listed(&self, claimed: &[PaneKey], workspaces: &[PathBuf]) -> Vec<ListedPane<'_>> {
        if !self.config.enabled {
            return Vec::new();
        }
        self.panes()
            .into_iter()
            .map(|(side, pane)| {
                (Self::key(side, &pane.terminal_id), pane.workspace(side, workspaces), pane)
            })
            .filter(|(key, workspace, _)| {
                !claimed.contains(key) && (workspace.is_some() || self.config.show_unmatched)
            })
            .map(|(key, workspace, pane)| ListedPane { workspace, key, pane })
            .collect()
    }

    fn default_side(&self) -> Result<Side, String> {
        let running: Vec<&Side> = self
            .sides
            .iter()
            .filter(|listing| !listing.sessions.is_empty())
            .map(|listing| &listing.side)
            .collect();
        match running.as_slice() {
            [side] => Ok((*side).clone()),
            [] => Err("no zellij session is running; start one, or name a side".to_string()),
            sides => Err(format!(
                "no zellij session is focused and {} are running one; name a side",
                sides.iter().map(|side| side.name()).collect::<Vec<_>>().join(" and ")
            )),
        }
    }

    fn gone_since(&self, side: &Side, terminal_id: &str, bound_at: Instant) -> Option<Instant> {
        let (session, _) = split_terminal_id(terminal_id)?;
        self.side(side)
            .filter(|listing| listing.sampled_at > bound_at && listing.lost(session, terminal_id))
            .map(|listing| listing.sampled_at)
    }

    fn pane_json(&self, side: &Side, terminal_id: &str, pane: Option<&Pane>) -> Value {
        json!({
            "name": MultiplexerKind::Zellij.to_string(),
            "side": side.name(),
            "session": split_terminal_id(terminal_id).map(|(session, _)| session),
            "terminal_id": terminal_id,
            "pane_id": pane.map(|pane| pane.pane_id.clone()),
            "tab_id": pane.and_then(|pane| pane.tab_id.clone()),
        })
    }

    /// zellij's detach chord is its own config's to say, and alacritree does
    /// not read that config, so the row stays quiet about it.
    fn managed(&self, _side: &Side, pane: Option<&Pane>) -> Managed {
        Managed {
            multiplexer: MultiplexerKind::Zellij,
            detach: None,
            shared_view: true,
            kind: None,
            title: pane.and_then(|pane| pane.title.clone()),
            mark: None,
        }
    }

    /// zellij reports no agent state, so no pane of its own ever asks for a
    /// mark; one handed over anyway reads as no reading.
    fn mark(&self, _side: &Side, status: PaneStatus) -> HarnessMark {
        HarnessMark { glyph: "·", tone: StateTone::Unclear, label: status.label() }
    }

    fn attaches_directly(&self, _side: &Side, _has_agent: bool) -> bool {
        false
    }

    fn open_directly(&self, _target: &PaneTarget) -> Option<Launch> {
        None
    }

    fn shared_view(&self, key: &PaneKey) -> Option<Launch> {
        let (session, _) = split_terminal_id(&key.terminal_id)?;
        let (program, argv) = attach(&self.program(&key.side), &key.side, session);
        Some(Launch { program, argv })
    }

    fn queue_attach(&mut self, key: PaneKey, _target: PaneTarget, request: AttachRequest) {
        if let Some(pending) = self.pending_attach.iter_mut().find(|p| p.key == key) {
            pending.request.waiters.extend(request.waiters);
            if request.focus.takes() && !pending.request.focus.takes() {
                pending.request.focus = request.focus;
                pending.job = None;
            }
            return;
        }
        self.pending_attach.push(PendingAttach { job: None, key, request });
    }

    /// One attach at a time, since each one's focus call moves the focus the
    /// next one's client opens on.
    fn poll_attach(&mut self) -> (Option<AttachAnswer>, bool) {
        let Some(head) = self.pending_attach.first() else { return (None, false) };
        let Some(job) = &head.job else {
            let key = head.key.clone();
            let focus = head.request.focus.takes();
            let program = self.program(&key.side);
            let job = jobs::pool().spawn(jobs::Priority::Interactive, move |_blocking| {
                let (session, pane_id) = split_terminal_id(&key.terminal_id)
                    .ok_or_else(|| format!("`{}` names no zellij pane", key.terminal_id))?;
                if focus {
                    focus_pane(&program, &key.side, session, pane_id)?;
                }
                let (program, argv) = attach(&program, &key.side, session);
                Ok(Launch { program, argv })
            });
            self.pending_attach[0].job = Some(job);
            return (None, false);
        };
        let launch = match job.poll() {
            Some(launch) => launch,
            None if job.failed() => Err("the zellij attach did not finish".to_string()),
            None => return (None, false),
        };
        let pending = self.pending_attach.remove(0);
        let starting = !self.pending_attach.is_empty();
        (Some(AttachAnswer { key: pending.key, request: pending.request, launch }), starting)
    }

    fn queue_create(&mut self, side: Side, cwd: Option<String>, request: CreateRequest) {
        let session = self.create_session(&side);
        let program = self.program(&side);
        let asked = side.clone();
        let focus = request.focus.takes();
        let job = jobs::pool().spawn(jobs::Priority::Interactive, move |_blocking| {
            create_pane(&program, &asked, &session?, cwd.as_deref(), focus)
        });
        self.pending_create.push(PendingCreate { job, side, request });
    }

    fn poll_create(&mut self) -> Option<CreateAnswer> {
        let pending = self.pending_create.first()?;
        let pane = match pending.job.poll() {
            Some(pane) => pane,
            None if pending.job.failed() => {
                Err("the zellij pane create did not finish".to_string())
            },
            None => return None,
        };
        let pending = self.pending_create.remove(0);
        Some(CreateAnswer { side: pending.side, request: pending.request, pane })
    }

    /// Each zellij client keeps its own focus, so a session keeps showing the
    /// pane its attach pointed it at with nothing to keep in step, and a move
    /// inside one client is that client's alone.
    fn sync_view(&mut self, _state: ViewState<'_>) -> ViewStep {
        ViewStep::default()
    }

    fn view_attached(&mut self, _id: SessionId, _key: &PaneKey) {}

    fn view_refused(&mut self, _key: &PaneKey) {}

    fn session_closed(&mut self, _id: SessionId, key: Option<&PaneKey>) {
        let Some(key) = key else { return };
        let (closed, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending_attach)
            .into_iter()
            .partition(|pending| &pending.key == key);
        self.pending_attach = kept;
        for pending in closed {
            for waiter in pending.request.waiters {
                let _ = waiter.send(Err("the session behind this pane was closed before the \
                                         attach finished"
                    .to_string()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::multiplexer::AttachFocus;

    fn pane(session: &str, id: u32, cwd: &str) -> Pane {
        Pane {
            terminal_id: terminal_id(session, id),
            pane_id: listing::pane_id(id),
            tab_id: Some("0".into()),
            kind: None,
            title: Some("shell".into()),
            status: None,
            focused: false,
            cwd: Some(cwd.into()),
            foreground_cwd: None,
        }
    }

    fn side(side: Side, sessions: &[&str], panes: Vec<Pane>, sampled_at: Instant) -> SideListing {
        let sessions: Vec<String> = sessions.iter().map(|s| (*s).to_string()).collect();
        SideListing { side, read: sessions.clone(), sessions, panes, sampled_at }
    }

    fn zellij(sides: Vec<SideListing>) -> Zellij {
        let mut zellij = Zellij::new(ZellijConfig::default());
        zellij.adopt_for_test(sides);
        zellij
    }

    #[test]
    fn zellij_is_off_unless_enabled() {
        assert!(!Zellij::new(ZellijConfig::default()).enabled());
    }

    /// zellij has no attach for one pane, whatever the side or the pane.
    #[test]
    fn every_attach_is_a_shared_view_of_the_pane_s_session() {
        let zellij = zellij(Vec::new());
        let key = Zellij::key(&Side::Wsl("d".into()), &terminal_id("work", 3));
        assert!(!zellij.attaches_directly(&key.side, true));
        assert_eq!(zellij.open_directly(&PaneTarget::unlisted(&key, "terminal_3")), None);
        let launch = zellij.shared_view(&key).expect("a zellij pane names its session");
        assert!(
            launch.argv.last().is_some_and(|script| script.ends_with("zellij attach work")),
            "{launch:?} does not attach the session",
        );
    }

    #[test]
    fn a_claimed_pane_is_not_listed() {
        let at = Instant::now();
        let zellij = zellij(vec![side(
            Side::Native,
            &["s"],
            vec![pane("s", 1, "/a"), pane("s", 2, "/a")],
            at,
        )]);
        let claimed = [Zellij::key(&Side::Native, &terminal_id("s", 1))];
        let listed = zellij.listed(&claimed, &[PathBuf::from("/a")]);
        let ids: Vec<&str> = listed.iter().map(|l| l.key.terminal_id.as_str()).collect();
        assert_eq!(ids, ["s/terminal_2"]);
        assert_eq!(listed[0].workspace, Some(PathBuf::from("/a")));
    }

    /// A pane whose session ended after the session was bound is gone; one
    /// in a listing sampled before the bind says nothing yet.
    #[test]
    fn a_pane_is_gone_only_by_a_listing_after_the_bind() {
        let bound_at = Instant::now();
        let later = bound_at + std::time::Duration::from_millis(1);
        let id = terminal_id("s", 1);
        let before = zellij(vec![side(Side::Native, &[], Vec::new(), bound_at)]);
        assert_eq!(before.gone_since(&Side::Native, &id, bound_at), None);
        let after = zellij(vec![side(Side::Native, &[], Vec::new(), later)]);
        assert_eq!(after.gone_since(&Side::Native, &id, bound_at), Some(later));
    }

    #[test]
    fn a_new_pane_goes_to_the_one_running_session() {
        let zellij = zellij(vec![side(Side::Native, &["only"], Vec::new(), Instant::now())]);
        assert_eq!(zellij.create_session(&Side::Native), Ok("only".to_string()));
    }

    /// Several running sessions leave no way to tell which one a create
    /// meant, so the answer says how to choose.
    #[test]
    fn a_new_pane_among_several_sessions_is_refused() {
        let zellij = zellij(vec![side(Side::Native, &["a", "b"], Vec::new(), Instant::now())]);
        let refusal = zellij.create_session(&Side::Native).unwrap_err();
        assert!(refusal.contains("[integrations.zellij] session"), "{refusal}");
    }

    #[test]
    fn a_configured_session_is_where_new_panes_go() {
        let mut zellij = zellij(vec![side(Side::Native, &["a", "b"], Vec::new(), Instant::now())]);
        zellij.config.session = Some("b".into());
        assert_eq!(zellij.create_session(&Side::Native), Ok("b".to_string()));
    }

    #[test]
    fn closing_a_session_answers_only_its_own_queued_attach() {
        let mut zellij = zellij(Vec::new());
        let first = Zellij::key(&Side::Native, &terminal_id("s", 1));
        let second = Zellij::key(&Side::Native, &terminal_id("s", 2));
        let (first_tx, first_rx) = mpsc::channel();
        let (second_tx, second_rx) = mpsc::channel();
        let request = |waiter| AttachRequest {
            workspace: None,
            previous: None,
            waiters: vec![waiter],
            focus: AttachFocus::Take,
        };
        let unlisted = |key: &PaneKey| PaneTarget::unlisted(key, "");
        zellij.queue_attach(first.clone(), unlisted(&first), request(first_tx));
        zellij.queue_attach(second.clone(), unlisted(&second), request(second_tx));

        zellij.session_closed(1, Some(&first));

        assert_eq!(zellij.pending_attach.len(), 1);
        assert_eq!(zellij.pending_attach[0].key, second);
        assert!(first_rx.try_recv().unwrap().is_err());
        assert!(second_rx.try_recv().is_err());
    }
}
