//! The multiplexers this build hosts panes from, dispatched by `match` rather
//! than a vtable.  What a multiplexer is asked lives in
//! `alacritree_multiplexer`; this module only decides which ones exist and
//! routes a request to the one that owns a pane.

use std::path::PathBuf;
use std::str::FromStr;

use alacritree_common::side::Side;
use alacritree_herdr::Herdr;
#[cfg(test)]
use alacritree_multiplexer::Scripted;
use alacritree_multiplexer::{
    ListedPane, MultiplexerKind, MultiplexerSession, Pane, PaneError, PaneKey,
    ambassador_impl_MultiplexerSession,
};
use alacritree_zellij::Zellij;
use ambassador::Delegate;

use crate::config::{BakedGlyph, DEFAULT_HERDR_ICON, DEFAULT_ZELLIJ_ICON, IntegrationsConfig};
use crate::session::SessionId;

/// The terminal multiplexers alacritree can host a pane from, each holding
/// its own listing and in-flight calls.
#[derive(Delegate)]
#[delegate(MultiplexerSession)]
// One of each is built for the app's lifetime, so the size spread between
// variants costs nothing.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Multiplexer {
    Herdr(Herdr),
    Zellij(Zellij),
    /// Answers from a script instead of a server, so app behaviour can be
    /// tested at the trait rather than through one multiplexer's wire format.
    #[cfg(test)]
    Scripted(Scripted),
}

impl Multiplexer {
    pub(crate) fn kind(&self) -> MultiplexerKind {
        match self {
            Self::Herdr(_) => MultiplexerKind::Herdr,
            Self::Zellij(_) => MultiplexerKind::Zellij,
            #[cfg(test)]
            Self::Scripted(_) => MultiplexerKind::Scripted,
        }
    }

    /// The glyph a row draws when the config leaves this multiplexer's icon
    /// blank.  Every `BakedGlyph` is declared through `baked_glyphs!`, so the
    /// baked font subset covers it.
    pub(crate) fn default_icon(&self) -> BakedGlyph {
        match self {
            Self::Herdr(_) => DEFAULT_HERDR_ICON,
            Self::Zellij(_) => DEFAULT_ZELLIJ_ICON,
            // A scripted row wants a neutral mark rather than one of its own.
            #[cfg(test)]
            Self::Scripted(_) => crate::config::DEFAULT_SESSION_ICON,
        }
    }

    /// The key a pane on `side` is known by here.
    pub(crate) fn key(&self, side: &Side, terminal_id: &str) -> PaneKey {
        PaneKey {
            multiplexer: self.kind(),
            side: side.clone(),
            terminal_id: terminal_id.to_string(),
        }
    }
}

/// Every multiplexer alacritree knows, in a fixed order, each configured from
/// its own `[integrations]` table.
pub(crate) struct Multiplexers(Vec<Multiplexer>);

impl Multiplexers {
    /// In the order of [`MultiplexerKind::real`], which is the order a
    /// listing is polled and drawn in.
    pub(crate) fn new(config: &IntegrationsConfig) -> Self {
        Self(vec![
            Multiplexer::Herdr(Herdr::new(config.herdr.clone())),
            Multiplexer::Zellij(Zellij::new(config.zellij.clone())),
        ])
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &Multiplexer> {
        self.0.iter()
    }

    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = &mut Multiplexer> {
        self.0.iter_mut()
    }

    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    /// The kind held at `at`.  A frame that has to reach `&mut self` inside
    /// its own loop walks positions rather than kinds, since the kind list is
    /// not the set that was built.
    pub(crate) fn kind_at(&self, at: usize) -> MultiplexerKind {
        self.0[at].kind()
    }

    pub(crate) fn get(&self, kind: MultiplexerKind) -> &Multiplexer {
        self.0.iter().find(|m| m.kind() == kind).expect("every kind is built in `new`")
    }

    pub(crate) fn get_mut(&mut self, kind: MultiplexerKind) -> &mut Multiplexer {
        self.0.iter_mut().find(|m| m.kind() == kind).expect("every kind is built in `new`")
    }

    pub(crate) fn any_enabled(&self) -> bool {
        self.iter().any(Multiplexer::enabled)
    }

    /// The reason a request naming no multiplexer is refused while every one
    /// is off.
    pub(crate) fn disabled_reason(&self) -> &'static str {
        alacritree_multiplexer::all_disabled_reason()
    }

    /// The multiplexer a request named, or `None` when it named none and any
    /// may answer.  A name that is no multiplexer, one that is switched off,
    /// and every one being off are all refusals.
    pub(crate) fn requested(
        &self,
        name: Option<&str>,
    ) -> Result<Option<MultiplexerKind>, PaneError> {
        let Some(name) = name else {
            return if self.any_enabled() { Ok(None) } else { Err(PaneError::AllDisabled) };
        };
        let kind = MultiplexerKind::from_str(name)
            .ok()
            .filter(|kind| MultiplexerKind::real().any(|real| real == *kind))
            .ok_or_else(|| PaneError::Unknown { name: name.to_string() })?;
        if self.get(kind).enabled() { Ok(Some(kind)) } else { Err(PaneError::Disabled(kind)) }
    }

    /// The multiplexer a request naming none goes to: the first one enabled.
    pub(crate) fn default_enabled(&self) -> Option<&Multiplexer> {
        self.iter().find(|m| m.enabled())
    }

    /// The pane `key` names, while its multiplexer still lists it.
    pub(crate) fn find(&self, key: &PaneKey) -> Option<&Pane> {
        self.get(key.multiplexer).find(&key.side, &key.terminal_id)
    }

    /// The pane a client named by side and terminal id, in the multiplexer it
    /// named or else in the first enabled one listing it.
    pub(crate) fn locate(
        &self,
        only: Option<MultiplexerKind>,
        side: &Side,
        terminal_id: &str,
    ) -> Option<(PaneKey, &Pane)> {
        self.iter()
            .filter(|m| m.enabled() && only.is_none_or(|kind| kind == m.kind()))
            .find_map(|m| m.find(side, terminal_id).map(|pane| (m.key(side, terminal_id), pane)))
    }

    /// Where a pane sits across every multiplexer's listing, in poll order.
    pub(crate) fn pane_index(&self, key: &PaneKey) -> Option<usize> {
        let mut before = 0;
        for multiplexer in self.iter() {
            if multiplexer.kind() == key.multiplexer {
                return multiplexer.pane_index(&key.side, &key.terminal_id).map(|at| before + at);
            }
            before += multiplexer.pane_count();
        }
        None
    }

    pub(crate) fn generation(&self) -> u64 {
        self.iter().fold(0, |sum, m| sum.wrapping_add(m.generation()))
    }

    /// Every listed pane no session holds, across the enabled multiplexers.
    pub(crate) fn listed(
        &self,
        claimed: &[PaneKey],
        workspaces: &[PathBuf],
    ) -> Vec<ListedPane<'_>> {
        self.iter().filter(|m| m.enabled()).flat_map(|m| m.listed(claimed, workspaces)).collect()
    }

    /// Hand every multiplexer the closed session, with the pane it held only
    /// to the one that owns it.
    pub(crate) fn session_closed(&mut self, id: SessionId, key: Option<&PaneKey>) {
        for multiplexer in self.iter_mut() {
            let owned = key.filter(|key| key.multiplexer == multiplexer.kind());
            multiplexer.session_closed(id, owned);
        }
    }

    /// Drop every real multiplexer, leaving the scripted one alone in the
    /// set.  A test about what the app does with a multiplexer then answers
    /// only for that, rather than also for which real one ships enabled.
    #[cfg(test)]
    pub(crate) fn only_scripted(&mut self) -> &mut Scripted {
        self.0.retain(|m| m.kind() == MultiplexerKind::Scripted);
        self.scripted_mut()
    }

    /// The scripted multiplexer, built on first use.  `new` cannot make one,
    /// since it builds only the real kinds.
    #[cfg(test)]
    pub(crate) fn scripted_mut(&mut self) -> &mut Scripted {
        if !self.0.iter().any(|m| m.kind() == MultiplexerKind::Scripted) {
            self.0.push(Multiplexer::Scripted(Scripted::default()));
        }
        let Some(Multiplexer::Scripted(scripted)) =
            self.0.iter_mut().find(|m| m.kind() == MultiplexerKind::Scripted)
        else {
            unreachable!("just pushed, and nothing else carries that kind")
        };
        scripted
    }

    #[cfg(test)]
    pub(crate) fn scripted(&self) -> &Scripted {
        let Some(Multiplexer::Scripted(scripted)) =
            self.0.iter().find(|m| m.kind() == MultiplexerKind::Scripted)
        else {
            panic!("`scripted_mut` builds it; call that first")
        };
        scripted
    }

    #[cfg(test)]
    pub(crate) fn herdr_mut_for_test(&mut self) -> &mut Herdr {
        let Multiplexer::Herdr(herdr) = self.get_mut(MultiplexerKind::Herdr) else {
            unreachable!("`get_mut` answers with the kind it was asked for")
        };
        herdr
    }

    #[cfg(test)]
    pub(crate) fn herdr_for_test(&self) -> &Herdr {
        let Multiplexer::Herdr(herdr) = self.get(MultiplexerKind::Herdr) else {
            unreachable!("`get` answers with the kind it was asked for")
        };
        herdr
    }

    #[cfg(test)]
    pub(crate) fn zellij_mut_for_test(&mut self) -> &mut Zellij {
        let Multiplexer::Zellij(zellij) = self.get_mut(MultiplexerKind::Zellij) else {
            unreachable!("`get_mut` answers with the kind it was asked for")
        };
        zellij
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each multiplexer that fronts a server is built once, so every kind a
    /// user can name resolves to one, in the order the kinds are declared.
    #[test]
    fn every_real_kind_has_a_multiplexer() {
        let all = Multiplexers::new(&IntegrationsConfig::default());
        let built: Vec<MultiplexerKind> = all.iter().map(Multiplexer::kind).collect();
        let real: Vec<MultiplexerKind> = MultiplexerKind::real().collect();
        assert_eq!(built, real);
    }

    /// The trait lives in another crate from this enum, so this is the check
    /// that ambassador's delegation reaches each backend rather than a
    /// default: the scripted one answers what its script says.
    #[test]
    fn a_call_on_the_enum_reaches_the_backend_it_holds() {
        let mut all = Multiplexers::new(&IntegrationsConfig::default());
        let scripted = all.scripted_mut();
        scripted.enable().attach_directly(true);
        scripted.set_panes(&Side::Native, vec![Scripted::pane("t1")]);
        let multiplexer = all.get(MultiplexerKind::Scripted);
        assert!(multiplexer.enabled());
        assert!(multiplexer.attaches_directly(&Side::Native, false));
        assert_eq!(multiplexer.pane_count(), 1);
        assert!(!all.get(MultiplexerKind::Zellij).enabled(), "zellij ships disabled");
    }

    /// The scripted kind is a test fixture, not something a config enables or
    /// a request reaches, so it stays out of both the built set and every
    /// message that enumerates multiplexers.
    #[test]
    fn the_scripted_kind_is_reachable_only_by_asking_for_it() {
        let mut all = Multiplexers::new(&IntegrationsConfig::default());
        assert!(!MultiplexerKind::real().any(|kind| kind == MultiplexerKind::Scripted));
        assert_eq!(all.len(), MultiplexerKind::real().count());
        assert_eq!(
            all.requested(Some("scripted")).unwrap_err().to_string(),
            "`scripted` is not a multiplexer, expected `herdr` or `zellij`"
        );
        assert!(!all.disabled_reason().contains("scripted"));

        all.scripted_mut().enable();
        assert_eq!(all.len(), MultiplexerKind::real().count() + 1);
        assert!(all.get(MultiplexerKind::Scripted).enabled());
    }

    #[test]
    #[ignore = "requires WSL"]
    fn multiplexer_command_keeps_the_probe_pid() {
        use alacritree_common::wsl_helper::{new_probe_key, wrap_exec_argv};
        let distro = alacritree_common::wsl::distros()
            .into_iter()
            .find(|d| d.is_default)
            .expect("a default distro");
        let key = new_probe_key();
        let (program, args) = Side::Wsl(distro.name).command("sh", &[
            "-c",
            r#"f=${XDG_RUNTIME_DIR:-/tmp}/alacritree/session-$1.pid; p=$(cat "$f") || exit 1; rm -f "$f"; printf '%s\n%s\n' "$$" "$p""#,
            "sh",
            &key,
        ]);
        let args = wrap_exec_argv(&program, &args, &key).expect("wrap multiplexer command");
        #[allow(clippy::disallowed_methods)] // A test waiting on its own child.
        let output = alacritree_common::command_ext::hidden(program)
            .args(args)
            .output()
            .expect("run in WSL");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let stdout = String::from_utf8(output.stdout).expect("PID output is UTF-8");
        let pids: Vec<_> = stdout.lines().collect();
        assert_eq!(pids.len(), 2, "command and probe PIDs: {stdout:?}");
        assert_eq!(pids[0], pids[1], "the probe must track the command, not its login shell");
    }
}
