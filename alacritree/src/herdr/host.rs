//! herdr as one of alacritree's multiplexers: the servers it polls, the calls
//! in flight to them, and which pane its shared view is showing.

use std::path::PathBuf;
use std::time::Instant;

use serde_json::{Value, json};

use super::view::{HerdrViewAction, HerdrViewFocus, HerdrViewSync, ViewInputs};
use super::{
    EndpointCache, Endpoints, Listing, Settings, attaches_directly, cli, focus_args, focus_pane,
    pane_key, program, unattached,
};
use crate::config::{BakedGlyph, DEFAULT_HERDR_ICON, HerdrConfig, IconStyle};
use crate::jobs;
use crate::multiplexer::{
    AttachAnswer, AttachRequest, CreateAnswer, CreateRequest, CreatedPane, Launch, ListedPane,
    Managed, MultiplexerKind, MultiplexerSession, Pane, PaneKey, PaneTarget, Side, ViewState,
    ViewStep,
};
use crate::session::SessionId;

/// A shared-view attach waiting on herdr.  The gesture answers with the argv
/// its client runs, so everything the session needs is in hand by the time it
/// opens.
pub(crate) struct PendingAttach {
    pub(crate) job: Option<jobs::Job<Result<Launch, String>>>,
    /// The pane to focus once the gesture runs.  A pane the listing has since
    /// dropped is focused as this said, since nothing newer says otherwise.
    pub(crate) target: PaneTarget,
    pub(crate) key: PaneKey,
    pub(crate) request: AttachRequest,
}

pub(crate) struct PendingCreate {
    pub(crate) job: jobs::Job<Result<CreatedPane, String>>,
    pub(crate) side: Side,
    pub(crate) request: CreateRequest,
}

pub(crate) struct Herdr {
    config: HerdrConfig,
    /// The herdr servers this app talks to: the native side plus one per
    /// running WSL distro, kept in step with which distros are up.
    endpoints: Endpoints,
    pending_attach: Vec<PendingAttach>,
    pending_create: Vec<PendingCreate>,
    /// The shared view herdr was last focused for, and the call still on its
    /// way, both owned by `sync_view`.
    focused_view: HerdrViewSync,
    view_focus: Option<HerdrViewFocus>,
}

impl Herdr {
    pub(crate) fn new(config: HerdrConfig) -> Self {
        Self {
            config,
            endpoints: Endpoints::default(),
            pending_attach: Vec::new(),
            pending_create: Vec::new(),
            focused_view: HerdrViewSync::default(),
            view_focus: None,
        }
    }

    fn cache(&self, side: &Side) -> Option<&EndpointCache> {
        self.endpoints.caches().iter().find(|cache| cache.side() == side)
    }

    /// What herdr's config says on `side`, once that endpoint's read has
    /// landed.
    fn settings(&self, side: &Side) -> Settings {
        self.cache(side).map(EndpointCache::settings).unwrap_or_default()
    }

    /// A gesture that timed out is the only sign of a herdr that hung with
    /// its streams still open, so that side's streams start over.
    fn note_gesture<T>(&mut self, side: &Side, result: &Result<T, String>) {
        if result.as_ref().is_err_and(|e| e.starts_with(cli::NO_ANSWER))
            && let Some(cache) = self.endpoints.cache_mut(side)
        {
            cache.restart();
        }
    }

    /// A user reaching for a side is the moment its herdr being up matters,
    /// so a side waiting out its backoff reconnects now.
    fn reconnect_now(&mut self, side: &Side) {
        if let Some(cache) = self.endpoints.cache_mut(side) {
            cache.reconnect_now();
        }
    }

    /// Where herdr's focus goes for the pane a session is bound to.  The
    /// displayed listing drops a pane with no agent in it unless panes are
    /// shown, so a bound shell pane is found through what the side's full
    /// listing last said about it.
    fn focus_target(&self, key: &PaneKey) -> Option<PaneTarget> {
        let cache = self.cache(&key.side)?;
        cache
            .agents()
            .iter()
            .find(|agent| agent.terminal_id == key.terminal_id)
            .or_else(|| cache.attachment_pane(&key.terminal_id).map(|pane| &pane.agent))
            .map(|agent| agent.target(&key.side))
    }

    #[cfg(test)]
    pub(crate) fn config_mut_for_test(&mut self) -> &mut HerdrConfig {
        &mut self.config
    }

    #[cfg(test)]
    pub(crate) fn caches_mut_for_test(&mut self) -> &mut Vec<EndpointCache> {
        self.endpoints.caches_mut_for_test()
    }

    /// Settle `side`'s listing from a raw `pane list` reply, as a poll would.
    #[cfg(test)]
    pub(crate) fn adopt_listing_for_test(&mut self, side: &Side, json: &str, at: Instant) {
        let display = Listing::wanted(self.config.show_panes);
        let caches = self.endpoints.caches_mut_for_test();
        if !caches.iter().any(|cache| cache.side() == side) {
            caches.push(EndpointCache::new(side.clone()));
        }
        caches.iter_mut().find(|cache| cache.side() == side).unwrap().complete_listing_for_test(
            Ok(json),
            Listing::Panes,
            display,
            at,
        );
    }

    #[cfg(test)]
    pub(crate) fn caches_for_test(&self) -> &[EndpointCache] {
        self.endpoints.caches()
    }

    #[cfg(test)]
    pub(crate) fn pending_attach_for_test(&self) -> &Vec<PendingAttach> {
        &self.pending_attach
    }

    #[cfg(test)]
    pub(crate) fn pending_attach_mut_for_test(&mut self) -> &mut Vec<PendingAttach> {
        &mut self.pending_attach
    }

    #[cfg(test)]
    pub(crate) fn pending_create_for_test(&self) -> &Vec<PendingCreate> {
        &self.pending_create
    }

    #[cfg(test)]
    pub(crate) fn pending_create_mut_for_test(&mut self) -> &mut Vec<PendingCreate> {
        &mut self.pending_create
    }

    #[cfg(test)]
    pub(crate) fn view_mut_for_test(&mut self) -> &mut HerdrViewSync {
        &mut self.focused_view
    }

    #[cfg(test)]
    pub(crate) fn view_focus_mut_for_test(&mut self) -> &mut Option<HerdrViewFocus> {
        &mut self.view_focus
    }
}

impl MultiplexerSession for Herdr {
    fn enabled(&self) -> bool {
        self.config.enabled
    }

    fn icon(&self) -> (&IconStyle, BakedGlyph) {
        (&self.config.icon, DEFAULT_HERDR_ICON)
    }

    fn poll(&mut self, attached: &dyn Fn(&Side) -> bool) {
        if !self.config.enabled {
            return;
        }
        self.endpoints.poll(Listing::wanted(self.config.show_panes), attached);
    }

    fn generation(&self) -> u64 {
        self.endpoints.generation()
    }

    fn panes(&self) -> Vec<(&Side, &Pane)> {
        self.endpoints
            .caches()
            .iter()
            .flat_map(|cache| cache.agents().iter().map(move |agent| (cache.side(), agent)))
            .collect()
    }

    fn pane_index(&self, side: &Side, terminal_id: &str) -> Option<usize> {
        let mut before = 0;
        for cache in self.endpoints.caches() {
            if cache.side() == side {
                let at = cache.agents().iter().position(|a| a.terminal_id == terminal_id)?;
                return Some(before + at);
            }
            before += cache.agents().len();
        }
        None
    }

    fn pane_count(&self) -> usize {
        self.endpoints.caches().iter().map(|cache| cache.agents().len()).sum()
    }

    fn find(&self, side: &Side, terminal_id: &str) -> Option<&Pane> {
        self.cache(side)?.agents().iter().find(|a| a.terminal_id == terminal_id)
    }

    fn retained(&self, side: &Side, terminal_id: &str) -> Option<(&Pane, bool)> {
        self.cache(side)?.attachment_pane(terminal_id).map(|pane| (&pane.agent, pane.current))
    }

    fn listed(&self, claimed: &[PaneKey], workspaces: &[PathBuf]) -> Vec<ListedPane<'_>> {
        if !self.config.enabled {
            return Vec::new();
        }
        let mut listed = Vec::new();
        for cache in self.endpoints.caches() {
            let side = cache.side();
            for pane in unattached(cache.agents(), side, claimed) {
                let workspace = pane.workspace(side, workspaces);
                if workspace.is_none() && !self.config.show_unmatched {
                    continue;
                }
                let key = pane_key(side.clone(), pane.terminal_id.clone());
                listed.push(ListedPane { workspace, key, pane });
            }
        }
        listed
    }

    fn default_side(&self) -> Result<Side, String> {
        // A cache holds a sample time only while it still holds a listing, so
        // a side whose rows are gone is not offered as the one a create meant.
        let answering: Vec<&Side> = self
            .endpoints
            .caches()
            .iter()
            .filter(|cache| cache.sampled_at().is_some())
            .map(EndpointCache::side)
            .collect();
        match answering.as_slice() {
            [side] => Ok((*side).clone()),
            [] => Err("no herdr server is answering; start one, or name a side".to_string()),
            sides => Err(format!(
                "no herdr session is focused and {} are answering; name one",
                sides.iter().map(|side| side.name()).collect::<Vec<_>>().join(" and ")
            )),
        }
    }

    fn gone_since(&self, side: &Side, terminal_id: &str, bound_at: Instant) -> Option<Instant> {
        self.cache(side)?
            .inventory()
            .filter(|inventory| {
                inventory.sampled_at > bound_at && !inventory.terminal_ids.contains(terminal_id)
            })
            .map(|inventory| inventory.sampled_at)
    }

    /// `pane_id` and `tab_id` are null when the listing does not carry the
    /// pane, which says the cache does not know right now rather than that
    /// the pane is gone.
    fn pane_json(&self, side: &Side, terminal_id: &str, pane: Option<&Pane>) -> Value {
        json!({
            "name": MultiplexerKind::Herdr.to_string(),
            "side": side.name(),
            "session": self.cache(side).and_then(EndpointCache::session_name),
            "terminal_id": terminal_id,
            "pane_id": pane.map(|pane| pane.pane_id.clone()),
            "tab_id": pane.and_then(|pane| pane.tab_id.clone()),
        })
    }

    fn managed(&self, side: &Side, pane: Option<&Pane>) -> Managed {
        let settings = self.settings(side);
        let kind = pane.and_then(|a| a.kind.clone());
        let title =
            pane.and_then(|a| a.title.clone()).filter(|t| Some(t.as_str()) != kind.as_deref());
        Managed {
            multiplexer: MultiplexerKind::Herdr,
            detach: settings.detach.clone(),
            shared_view: !self.attaches_directly(side, pane.is_none_or(|a| a.status.is_some())),
            status: pane.and_then(|a| a.status),
            kind,
            title,
        }
    }

    fn attaches_directly(&self, side: &Side, has_agent: bool) -> bool {
        attaches_directly(side, self.config.attach, has_agent)
    }

    fn open_directly(&self, target: &PaneTarget) -> Option<Launch> {
        if !self.attaches_directly(&target.side, target.has_agent) {
            return None;
        }
        let args = cli::attach_args(&target.pane_id);
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        let (program, argv) = target.side.command(&program(&target.side), &borrowed);
        Some(Launch { program, argv })
    }

    /// herdr runs one session per server, so the side alone names the view.
    fn shared_view(&self, key: &PaneKey) -> Option<Launch> {
        let side = &key.side;
        let name = self.cache(side).and_then(EndpointCache::session_name)?;
        let (program, argv) = side.command(&program(side), &["session", "attach", &name]);
        Some(Launch { program, argv })
    }

    /// Every one of herdr's app clients draws the same focused pane, so a
    /// shared view shows a row's pane only while herdr is focused there.
    fn queue_attach(&mut self, key: PaneKey, target: PaneTarget, request: AttachRequest) {
        self.reconnect_now(&key.side);
        if let Some(pending) = self.pending_attach.iter_mut().find(|p| p.key == key) {
            pending.request.waiters.extend(request.waiters);
            if request.focus.takes() && !pending.request.focus.takes() {
                pending.request.focus = request.focus;
                // A background gesture changes nothing in herdr, so dropping
                // it and asking again with focus is safe.
                pending.job = None;
            }
            return;
        }
        self.pending_attach.push(PendingAttach { job: None, target, key, request });
    }

    fn poll_attach(&mut self) -> (Option<AttachAnswer>, bool) {
        // Attach gestures and focus calls both change herdr's global focus,
        // so only one may be in flight.
        if self.view_focus.is_some() || self.pending_attach.is_empty() {
            return (None, false);
        }
        let mut pending = self.pending_attach.remove(0);
        let answer = match &pending.job {
            Some(job) => match job.poll() {
                Some(launch) => Some(launch),
                None if job.failed() => Some(Err("the herdr attach did not finish".to_string())),
                None => None,
            },
            None => {
                let name = self.cache(&pending.key.side).and_then(EndpointCache::session_name);
                let side = pending.key.side.clone();
                let target = self
                    .find(&side, &pending.key.terminal_id)
                    .map_or_else(|| pending.target.clone(), |agent| agent.target(&side));
                let focus = pending.request.focus.takes();
                pending.job =
                    Some(jobs::pool().spawn(jobs::Priority::Interactive, move |_blocking| {
                        let focus = focus.then(|| focus_args(&target));
                        cli::herdr_attach_gesture(&target.side, focus.as_deref(), name)
                            .map(|(program, argv)| Launch { program, argv })
                    }));
                None
            },
        };
        let answer = match answer {
            Some(launch) => {
                self.note_gesture(&pending.key.side, &launch);
                Some(AttachAnswer { key: pending.key, request: pending.request, launch })
            },
            None => {
                self.pending_attach.insert(0, pending);
                None
            },
        };
        let starting = self.pending_attach.first().is_some_and(|pending| pending.job.is_none());
        (answer, starting)
    }

    fn queue_create(&mut self, side: Side, cwd: Option<String>, request: CreateRequest) {
        self.reconnect_now(&side);
        let asked = side.clone();
        let focus = request.focus.takes();
        let job = jobs::pool().spawn(jobs::Priority::Interactive, move |_blocking| {
            cli::create_pane(&asked, cwd, focus)
        });
        self.pending_create.push(PendingCreate { job, side, request });
    }

    fn poll_create(&mut self) -> Option<CreateAnswer> {
        if self.pending_create.is_empty() {
            return None;
        }
        let pending = self.pending_create.remove(0);
        let pane = match pending.job.poll() {
            Some(pane) => pane,
            None if pending.job.failed() => Err("the herdr pane create did not finish".to_string()),
            None => {
                self.pending_create.insert(0, pending);
                return None;
            },
        };
        self.note_gesture(&pending.side, &pane);
        Some(CreateAnswer { side: pending.side, request: pending.request, pane })
    }

    /// Local session switches focus herdr; a settled view follows later
    /// focus changes made inside herdr.  Listings started before our focus
    /// landed cannot reverse the user's session selection.
    fn sync_view(&mut self, state: ViewState<'_>) -> ViewStep {
        let action = self.focused_view.next(ViewInputs {
            active: state.active,
            follow: self.config.follow_focus,
            caches: self.endpoints.caches(),
            attentive: state.attentive,
            busy: self.view_focus.is_some() || !self.pending_attach.is_empty(),
            now: state.now,
            last_direct_input: state.last_direct_input,
        });
        if let Some(pending) = self.view_focus.take() {
            if !(state.is_open)(pending.session) {
                return ViewStep { follow: None, repaint: true };
            }
            match pending.job.poll() {
                Some(result) => {
                    self.note_gesture(&pending.key.side, &result);
                    let succeeded = result.is_ok();
                    if let Err(e) = result {
                        log::warn!("{e}");
                    }
                    if succeeded {
                        self.focused_view.moved_focus(&pending.key, Instant::now());
                    }
                    self.focused_view.settled(pending.session, succeeded, Instant::now());
                },
                None if pending.job.failed() => {
                    self.focused_view.settled(pending.session, false, Instant::now());
                },
                None => self.view_focus = Some(pending),
            }
            return ViewStep { follow: None, repaint: self.view_focus.is_none() };
        }
        match action {
            Some(HerdrViewAction::Focus(id)) => {
                let Some(key) = state.active.and_then(|(_, key, _)| key) else {
                    return ViewStep::default();
                };
                let Some(target) = self.focus_target(key) else { return ViewStep::default() };
                let focus = focus_args(&target);
                let side = key.side.clone();
                let job = jobs::pool()
                    .spawn(jobs::Priority::Interactive, move |_blocking| focus_pane(&side, &focus));
                self.view_focus = Some(HerdrViewFocus { session: id, key: key.clone(), job });
                ViewStep::default()
            },
            Some(HerdrViewAction::Follow(key)) => ViewStep { follow: Some(key), repaint: false },
            None => ViewStep::default(),
        }
    }

    fn view_attached(&mut self, id: SessionId, key: &PaneKey) {
        self.focused_view.attached(id, Some(key), Instant::now());
    }

    fn view_refused(&mut self, key: &PaneKey) {
        self.focused_view.moved_focus(key, Instant::now());
    }

    fn session_closed(&mut self, id: SessionId, key: Option<&PaneKey>) {
        if let Some(key) = key {
            // A plain `retain` would drop a queued attach's waiters with it,
            // leaving a parked client to time out rather than learn why.
            let (removed, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending_attach)
                .into_iter()
                .partition(|pending| &pending.key == key);
            self.pending_attach = kept;
            for pending in removed {
                for waiter in pending.request.waiters {
                    let _ = waiter.send(Err("the session behind this pane was closed before the \
                                             attach finished"
                        .to_string()));
                }
            }
        }
        if self.view_focus.as_ref().is_some_and(|pending| pending.session == id) {
            self.view_focus = None;
        }
        self.focused_view.closed(id, key, Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::config::AttachMode;
    use crate::multiplexer::AttachFocus;

    fn herdr(attach: AttachMode) -> Herdr {
        Herdr::new(HerdrConfig { attach, ..HerdrConfig::default() })
    }

    fn pane(side: Side, has_agent: bool) -> PaneTarget {
        PaneTarget { side, pane_id: "w1:p1".into(), tab_id: Some("w1:t1".into()), has_agent }
    }

    fn request(waiters: Vec<mpsc::Sender<crate::ipc::protocol::IpcResult>>) -> AttachRequest {
        AttachRequest { workspace: None, previous: None, waiters, focus: AttachFocus::Take }
    }

    /// A pane with an agent on a side that can hand one over opens directly,
    /// and the same pane with no agent does not, because every `herdr agent`
    /// subcommand resolves through a registry holding nothing for it.
    #[test]
    fn a_pane_with_no_agent_is_never_opened_directly() {
        let herdr = herdr(AttachMode::Agent);
        let wsl = Side::Wsl("d".into());
        let launch = herdr
            .open_directly(&pane(wsl.clone(), true))
            .expect("a WSL pane with an agent hands the pane over");
        assert!(
            launch.argv.last().is_some_and(|script| script.ends_with("agent attach 'w1:p1'")),
            "{launch:?} does not attach the pane",
        );
        assert_eq!(herdr.open_directly(&pane(wsl, false)), None);
    }

    /// A title saying no more than the kind is dropped, so the row names the
    /// agent once rather than twice.
    #[test]
    fn a_title_repeating_the_kind_is_not_reported() {
        let herdr = herdr(AttachMode::Agent);
        let repeated = crate::test_util::titled_agent(Some("codex"), Some("codex"));
        assert_eq!(herdr.managed(&Side::Native, Some(&repeated)).title, None);

        let distinct = crate::test_util::titled_agent(Some("codex"), Some("primary"));
        assert_eq!(herdr.managed(&Side::Native, Some(&distinct)).title.as_deref(), Some("primary"));
    }

    /// A row shares herdr's view wherever a direct attach is impossible, and
    /// on Windows a native pane is exactly that case.
    #[test]
    fn a_row_shares_the_view_wherever_a_pane_cannot_be_handed_over() {
        let herdr = herdr(AttachMode::Agent);
        let agent = crate::test_util::listed_agent(None);

        let native = herdr.managed(&Side::Native, Some(&agent));
        assert_eq!(native.shared_view, cfg!(windows));

        let wsl = herdr.managed(&Side::Wsl("d".into()), Some(&agent));
        assert!(!wsl.shared_view);
    }

    /// The configured attach mode outranks capability in one direction only:
    /// asking for a shared view always gets one, and no user is handed a
    /// direct attach they did not ask for.
    #[test]
    fn asking_for_a_shared_view_never_opens_a_pane_directly() {
        let herdr = herdr(AttachMode::Session);
        for side in [Side::Native, Side::Wsl("d".into())] {
            let target = pane(side, true);
            assert_eq!(herdr.open_directly(&target), None, "{target:?} was handed over unasked");
        }
    }

    #[test]
    fn closing_a_session_answers_only_its_own_queued_attach() {
        let mut herdr = herdr(AttachMode::Agent);
        let first_id = 1;
        let first = pane_key(Side::Native, "first".into());
        let second = pane_key(Side::Native, "second".into());
        let (first_tx, first_rx) = mpsc::channel();
        let (second_tx, second_rx) = mpsc::channel();
        herdr.queue_attach(
            first.clone(),
            PaneTarget::unlisted(&first, "w1:p1"),
            request(vec![first_tx]),
        );
        herdr.queue_attach(
            second.clone(),
            PaneTarget::unlisted(&second, "w1:p2"),
            request(vec![second_tx]),
        );
        herdr.view_focus = Some(HerdrViewFocus {
            session: first_id,
            key: first.clone(),
            job: jobs::Job::ready(Ok(())),
        });

        herdr.session_closed(first_id, Some(&first));

        assert!(herdr.view_focus.is_none());
        assert_eq!(herdr.pending_attach.len(), 1);
        assert_eq!(herdr.pending_attach[0].key, second);
        assert_eq!(
            first_rx.try_recv().unwrap(),
            Err("the session behind this pane was closed before the attach finished".to_string())
        );
        assert!(second_rx.try_recv().is_err());
    }

    /// A created pane runs a shell, and the displayed listing drops a pane
    /// with no agent in it unless panes are shown.  Coming back to that pane's
    /// session has to find its tab through the side's full listing, or herdr
    /// goes on showing whatever it last focused.
    #[test]
    fn a_bound_shell_pane_is_refocused_through_its_tab() {
        let mut herdr = herdr(AttachMode::Agent);
        assert!(!herdr.config.show_panes, "the default display");
        let side = Side::Native;
        herdr.adopt_listing_for_test(
            &side,
            r#"{"result":{"panes":[
            {"terminal_id":"term-shell","pane_id":"w1:p2","tab_id":"w1:t2","agent_status":"unknown"}
        ]}}"#,
            Instant::now(),
        );
        let key = pane_key(side, "term-shell".into());
        assert!(herdr.find(&key.side, &key.terminal_id).is_none(), "the listing shows the shell");

        let target = herdr.focus_target(&key).expect("a bound shell pane has nowhere to focus");

        assert_eq!(focus_args(&target), ["tab", "focus", "w1:t2"]);
    }

    /// A session alacritree still holds open after herdr stopped listing its
    /// pane has no state and no name left to report, but it is still herdr's
    /// and the user still has to know how to leave it.
    #[test]
    fn an_unlisted_pane_keeps_its_way_out() {
        let side = Side::Wsl("d".into());
        let mut herdr = herdr(AttachMode::Agent);
        let mut cache = EndpointCache::new(side.clone());
        cache.set_settings_for_test(Settings {
            detach: Some("Ctrl+B q".into()),
            ..Settings::default()
        });
        herdr.caches_mut_for_test().push(cache);
        let managed = herdr.managed(&side, None);
        assert_eq!(managed.detach.as_deref(), Some("Ctrl+B q"));
        assert_eq!((managed.status, managed.kind, managed.title), (None, None, None));
    }
}
