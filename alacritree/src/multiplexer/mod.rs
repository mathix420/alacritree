//! Which terminal multiplexer owns a pane, and what alacritree asks it in
//! order to host one.
//!
//! Everything the app needs from a multiplexer goes behind
//! [`MultiplexerSession`]: listing its panes, attaching to one, creating one,
//! and keeping its focus in step with the session on screen.  Each variant of
//! [`Multiplexer`] owns its own polling and in-flight calls, so another
//! multiplexer is a new variant and a new module, with nothing to change in
//! the app.

mod model;

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::Instant;

use enum_dispatch::enum_dispatch;
use serde_json::Value;
use strum::{Display, EnumDiscriminants, EnumIter, EnumString, IntoEnumIterator};

pub(crate) use self::model::{
    AttachAnswer, AttachFocus, AttachRequest, CreateAnswer, CreateRequest, HarnessMark, ListedPane,
    Managed, StateTone, ViewState, ViewStep,
};
pub use self::model::{Pane, PaneKey, PaneStatus};
use crate::config::{BakedGlyph, IconStyle, IntegrationsConfig};
use crate::herdr::Herdr;
use crate::session::SessionId;
use crate::wsl;
use crate::zellij::Zellij;

/// Which server a pane belongs to.  Two servers on one machine cannot see
/// each other, so this is part of a pane's identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Side {
    Native,
    /// Named distro, as `wsl.exe -d` spells it.
    Wsl(String),
}

impl Side {
    /// How a side is spelled outside the process: `native`, or `wsl:<distro>`
    /// as `wsl.exe -d` names it.  Two servers on one machine cannot see each
    /// other, so a pane named to a client without its side is not named at
    /// all.
    pub fn name(&self) -> String {
        match self {
            Self::Native => "native".to_string(),
            Self::Wsl(distro) => format!("wsl:{distro}"),
        }
    }

    /// Read back what `name` wrote.  A `wsl:` with nothing after it names no
    /// server, so it is refused rather than resolving to a distro called the
    /// empty string.
    pub fn parse(name: &str) -> Option<Self> {
        if name == "native" {
            return Some(Self::Native);
        }
        match name.strip_prefix("wsl:") {
            None | Some("") => None,
            Some(distro) => Some(Self::Wsl(distro.to_string())),
        }
    }

    /// How a row names this side.  `None` on the native one, whose name would
    /// be the same word on every row of a machine that has only it.
    pub fn label(&self) -> Option<String> {
        match self {
            Self::Native => None,
            Self::Wsl(distro) => Some(format!("wsl:{distro}")),
        }
    }

    /// Program and argv that run `program <args>` on this side.  WSL goes
    /// through a login shell because these binaries install to
    /// `~/.local/bin`, which is not on the PATH `wsl.exe -e` inherits.
    pub fn command(&self, program: &str, args: &[&str]) -> (String, Vec<String>) {
        match self {
            Self::Native => (program.to_string(), args.iter().map(|a| (*a).to_string()).collect()),
            Self::Wsl(distro) => {
                let script = std::iter::once(sh_quote(program))
                    .chain(args.iter().map(|a| sh_quote(a)))
                    .collect::<Vec<_>>()
                    .join(" ");
                // The login shell supplies PATH; exec preserves the PID
                // recorded by the foreground probe.
                wsl::exec_invocation(distro, &["sh", "-lc", &format!("exec {script}")])
            },
        }
    }

    /// The directory a new pane opens in, spelled where the multiplexer
    /// resolves it: the distro's own path on a WSL side, the Windows path on
    /// the native one.  `None` leaves the choice to the multiplexer.  A
    /// workspace with no spelling inside the distro is an `Err`, since a pane
    /// opened anywhere else would still have its session filed under it.
    pub(crate) fn cwd_for(
        &self,
        workspace: Option<&std::path::Path>,
    ) -> Result<Option<String>, String> {
        let Some(path) = workspace else { return Ok(None) };
        match self {
            Self::Native => Ok(Some(path.display().to_string())),
            Self::Wsl(distro) => wsl::windows_to_linux(path).map(Some).ok_or_else(|| {
                format!("{} has no path inside the {distro} distro", path.display())
            }),
        }
    }
}

/// Single-quote a POSIX argument, since WSL invocations are one `sh -lc`
/// string rather than an argv.
fn sh_quote(arg: &str) -> String {
    if !arg.is_empty() && arg.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=".contains(c)) {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// A pane alacritree wants a session on, in the terms the multiplexer that
/// owns it uses.  `pane_id` is positional and changes when a pane moves,
/// which is why it is not the pane's identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneTarget {
    pub side: Side,
    pub pane_id: String,
    /// The tab holding the pane, where the multiplexer reports one.
    pub tab_id: Option<String>,
    /// Whether the multiplexer reports an agent in this pane.  Some of them
    /// resolve a target through an agent registry, which holds nothing for a
    /// pane running a plain shell, so such a pane is reached another way.
    pub has_agent: bool,
}

impl PaneTarget {
    /// A pane the listing no longer carries.  Claiming an agent is in it
    /// keeps every caller on the path it took before the pane went.
    pub(crate) fn unlisted(key: &PaneKey, pane_id: &str) -> Self {
        Self { side: key.side.clone(), pane_id: pane_id.to_string(), tab_id: None, has_agent: true }
    }
}

/// A program and its argv, ready to be a session's shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Launch {
    pub program: String,
    pub argv: Vec<String>,
}

/// A pane a multiplexer has just made.  Every id comes back because each
/// answers a different question: `terminal_id` is the identity a session is
/// keyed on and survives the pane moving, `pane_id` is what an attach is
/// pointed at, and `tab_id` is how a pane with no agent in it is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedPane {
    pub terminal_id: String,
    pub pane_id: String,
    pub tab_id: String,
}

/// What the app asks of a multiplexer.  Every process call runs on the job
/// pool from inside the implementation, so none of these blocks a frame.
#[enum_dispatch]
pub(crate) trait MultiplexerSession {
    /// Whether the user has this multiplexer turned on.  Off stops the
    /// polling too, since the subprocesses are the whole cost.
    fn enabled(&self) -> bool;

    /// The glyph a row names this multiplexer with, and the one it falls back
    /// to when the config leaves it blank.
    fn icon(&self) -> (&IconStyle, BakedGlyph);

    /// Refresh the listing on this multiplexer's own clock.  `attached` says
    /// whether a session holds a pane on a side.
    fn poll(&mut self, attached: &dyn Fn(&Side) -> bool);

    /// One number standing for everything the listing draws, so a frame can
    /// tell a change with a `u64` compare.
    fn generation(&self) -> u64;

    /// Every pane the listing carries, in the order it was polled.
    fn panes(&self) -> Vec<(&Side, &Pane)>;

    /// Where a pane sits in `panes`.
    fn pane_index(&self, side: &Side, terminal_id: &str) -> Option<usize>;

    fn pane_count(&self) -> usize;

    /// The pane behind `(side, terminal_id)`, while the listing still has it.
    fn find(&self, side: &Side, terminal_id: &str) -> Option<&Pane>;

    /// What the listing last said about a pane a session holds, and whether
    /// that is still current.  A pane the displayed listing drops is still
    /// described here while it lives.
    fn retained(&self, side: &Side, terminal_id: &str) -> Option<(&Pane, bool)>;

    /// The panes no session in `claimed` holds, each with the workspace it
    /// belongs under.  A pane matching no workspace is left out unless the
    /// user asked to see those under Home.
    fn listed(&self, claimed: &[PaneKey], workspaces: &[PathBuf]) -> Vec<ListedPane<'_>>;

    /// The side a create that named none happens on, when only one server is
    /// answering.  `Err` names every side it could have meant.
    fn default_side(&self) -> Result<Side, String>;

    /// When the listing last showed `terminal_id` gone from a side that
    /// reported it after `bound_at`.
    fn gone_since(&self, side: &Side, terminal_id: &str, bound_at: Instant) -> Option<Instant>;

    /// Where a pane lives, in the fields an attach takes back.
    fn pane_json(&self, side: &Side, terminal_id: &str, pane: Option<&Pane>) -> Value;

    /// How a row describes a pane on `side`.  `pane` is `None` once the
    /// listing stops carrying it.
    fn managed(&self, side: &Side, pane: Option<&Pane>) -> Managed;

    /// The mark this multiplexer paints for `status` on `side`.
    fn mark(&self, side: &Side, status: PaneStatus) -> HarnessMark;

    /// Whether opening a pane's row attaches to that pane on its own rather
    /// than sharing the multiplexer's whole view.
    fn attaches_directly(&self, side: &Side, has_agent: bool) -> bool;

    /// The command that opens a session already showing `target`, when this
    /// multiplexer can hand one pane over.  `None` means the pane is reachable
    /// only by sharing the whole view, which `queue_attach` prepares.
    fn open_directly(&self, target: &PaneTarget) -> Option<Launch>;

    /// The command that shares the whole view holding `key`'s pane as it
    /// stands, with no focus call first.
    fn shared_view(&self, key: &PaneKey) -> Option<Launch>;

    /// Start preparing a shared view of `target`.  A second request for the
    /// same pane joins the first.
    fn queue_attach(&mut self, key: PaneKey, target: PaneTarget, request: AttachRequest);

    /// The first queued attach, once it has an answer.  `repaint` is set when
    /// a queued attach still has to start.
    fn poll_attach(&mut self) -> (Option<AttachAnswer>, bool);

    /// Start opening a pane on `side` in `cwd`, spelled in the side's own
    /// terms.
    fn queue_create(&mut self, side: Side, cwd: Option<String>, request: CreateRequest);

    /// The first queued create, once it has an answer.
    fn poll_create(&mut self) -> Option<CreateAnswer>;

    /// Keep the multiplexer's focus on the session on screen, and report a
    /// move the user made inside the multiplexer for the app to follow.
    fn sync_view(&mut self, state: ViewState<'_>) -> ViewStep;

    /// A session now shows `key`'s pane after an attach or a follow.
    fn view_attached(&mut self, id: SessionId, key: &PaneKey);

    /// The app would not follow to `key`, so the same move is not proposed
    /// again.
    fn view_refused(&mut self, key: &PaneKey);

    /// A session closed.  `key` is the pane it held, when this multiplexer
    /// owns it; clients waiting on an attach to that pane are told why.
    fn session_closed(&mut self, id: SessionId, key: Option<&PaneKey>);
}

/// The terminal multiplexers alacritree can host a pane from, each holding
/// its own listing and in-flight calls.
#[enum_dispatch(MultiplexerSession)]
#[derive(EnumDiscriminants)]
#[strum_discriminants(
    name(MultiplexerKind),
    derive(Hash, Display, EnumString, EnumIter),
    vis(pub),
    strum(serialize_all = "lowercase")
)]
// One of each is built for the app's lifetime, so the size spread between
// variants costs nothing.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Multiplexer {
    Herdr(Herdr),
    Zellij(Zellij),
}

impl MultiplexerKind {
    /// Why a request that named this multiplexer is refused while it is off.
    /// Alone among the refusals, this one is worth retrying after a config
    /// change.
    pub(crate) fn disabled_reason(self) -> String {
        format!("the {self} integration is disabled ([integrations.{self}] enabled)")
    }
}

impl Multiplexer {
    fn new(kind: MultiplexerKind, config: &IntegrationsConfig) -> Self {
        match kind {
            MultiplexerKind::Herdr => Herdr::new(config.herdr.clone()).into(),
            MultiplexerKind::Zellij => Zellij::new(config.zellij.clone()).into(),
        }
    }

    pub(crate) fn kind(&self) -> MultiplexerKind {
        self.into()
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
    pub(crate) fn new(config: &IntegrationsConfig) -> Self {
        Self(MultiplexerKind::iter().map(|kind| Multiplexer::new(kind, config)).collect())
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &Multiplexer> {
        self.0.iter()
    }

    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = &mut Multiplexer> {
        self.0.iter_mut()
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
        static REASON: OnceLock<String> = OnceLock::new();
        REASON.get_or_init(|| {
            let tables: Vec<String> =
                MultiplexerKind::iter().map(|kind| format!("[integrations.{kind}]")).collect();
            format!("every multiplexer integration is disabled ({} enabled)", tables.join(" or "))
        })
    }

    /// The multiplexer a request named, or `None` when it named none and any
    /// may answer.  A name that is no multiplexer, one that is switched off,
    /// and every one being off are all refusals.
    pub(crate) fn requested(&self, name: Option<&str>) -> Result<Option<MultiplexerKind>, String> {
        let Some(name) = name else {
            return if self.any_enabled() {
                Ok(None)
            } else {
                Err(self.disabled_reason().to_string())
            };
        };
        let kind = MultiplexerKind::from_str(name).map_err(|_| {
            let known: Vec<String> = MultiplexerKind::iter().map(|k| format!("`{k}`")).collect();
            format!("`{name}` is not a multiplexer, expected {}", known.join(" or "))
        })?;
        if self.get(kind).enabled() { Ok(Some(kind)) } else { Err(kind.disabled_reason()) }
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
    use std::str::FromStr;

    use super::*;

    /// A side has two spellings and they are not interchangeable: `label` is
    /// a row's word for it and stays silent on the native side, while a
    /// client that cannot see the row needs the side named every time.
    #[test]
    fn a_side_names_itself_on_both_sides_of_the_wire() {
        assert_eq!(Side::Native.name(), "native");
        assert_eq!(Side::Wsl("Ubuntu-24.04".into()).name(), "wsl:Ubuntu-24.04");
    }

    /// A client names a pane by the side it read out of a listing, so a side
    /// that does not survive the round trip points an attach at the wrong
    /// server or at none.
    #[test]
    fn a_side_reads_back_as_the_side_it_spelled() {
        for side in [Side::Native, Side::Wsl("Ubuntu-24.04".into())] {
            assert_eq!(Side::parse(&side.name()), Some(side));
        }
    }

    #[test]
    fn a_side_that_names_no_server_is_refused() {
        for name in ["", "wsl", "wsl:", "Native", "tmux:0"] {
            assert_eq!(Side::parse(name), None, "{name} named a server");
        }
    }

    #[test]
    fn native_runs_the_program_directly() {
        let (program, args) = Side::Native.command("herdr", &["agent", "list"]);
        assert_eq!(program, "herdr");
        assert_eq!(args, vec!["agent", "list"]);
    }

    /// These binaries install to ~/.local/bin, which reaches PATH only under
    /// a login shell.  `wsl.exe -e herdr` fails with execvpe ENOENT.
    #[test]
    fn wsl_wraps_in_a_login_shell() {
        let side = Side::Wsl("kali-linux".into());
        let (program, args) = side.command("herdr", &["agent", "list"]);
        assert_eq!(program, "wsl.exe");
        assert_eq!(args, vec!["-d", "kali-linux", "--exec", "sh", "-lc", "exec herdr agent list"]);
    }

    #[test]
    fn wsl_quotes_arguments_that_need_it() {
        let side = Side::Wsl("d".into());
        let (_, args) = side.command("herdr", &["agent", "attach", "w1:p1"]);
        assert_eq!(args.last().unwrap(), "exec herdr agent attach 'w1:p1'");
    }

    /// A client reads a multiplexer's name off a reply and may send it back,
    /// so the two spellings have to agree.  `herdr` is lowercase because that
    /// is the string already on the wire.
    #[test]
    fn a_multiplexer_reads_back_as_the_name_it_spelled() {
        assert_eq!(MultiplexerKind::Herdr.to_string(), "herdr");
        assert_eq!(MultiplexerKind::Zellij.to_string(), "zellij");
        for kind in MultiplexerKind::iter() {
            assert_eq!(MultiplexerKind::from_str(&kind.to_string()), Ok(kind));
        }
    }

    /// The multiplexer resolves the directory where it runs, so a WSL side is
    /// handed the distro's own spelling of the workspace and never the
    /// Windows path the sidebar holds.
    #[cfg(windows)]
    #[test]
    fn a_new_pane_opens_in_the_workspace_spelled_for_its_own_side() {
        let workspace = PathBuf::from(r"\\wsl.localhost\ubuntu\home\dev\repo");
        assert_eq!(
            Side::Wsl("ubuntu".into()).cwd_for(Some(&workspace)),
            Ok(Some("/home/dev/repo".to_string()))
        );
        assert_eq!(
            Side::Native.cwd_for(Some(&workspace)),
            Ok(Some(workspace.display().to_string()))
        );
    }

    /// The home workspace names no directory, so the multiplexer picks its own
    /// default rather than being handed an empty path.
    #[test]
    fn a_new_pane_in_the_home_workspace_names_no_directory() {
        assert_eq!(Side::Native.cwd_for(None), Ok(None));
        assert_eq!(Side::Wsl("ubuntu".into()).cwd_for(None), Ok(None));
    }

    /// Each multiplexer is built once, so every kind resolves to one.
    #[test]
    fn every_kind_has_a_multiplexer() {
        let all = Multiplexers::new(&IntegrationsConfig::default());
        for kind in MultiplexerKind::iter() {
            assert_eq!(all.get(kind).kind(), kind);
        }
    }
}
