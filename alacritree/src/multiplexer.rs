//! Which terminal multiplexer owns a pane, and what alacritree asks it in
//! order to host one.
//!
//! Only three of those questions have answers that differ between
//! multiplexers, and they go behind [`MultiplexerSession`], so a second
//! multiplexer is a new [`Multiplexer`] variant rather than a new branch at
//! every call site.
//! Everything else stays in that multiplexer's own module, where its shape is
//! honest about having exactly one example.

use enum_dispatch::enum_dispatch;
use strum::{Display, EnumIter, EnumString};

use crate::config::AttachMode;
use crate::{herdr, wsl};

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
                // `--exec` hands wsl.exe a bare program lookup, and these
                // binaries install off that PATH; routing through `sh -lc`
                // sources the login shell that puts them back.
                wsl::exec_invocation(distro, &["sh", "-lc", &script])
            },
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

/// A program and its argv, ready to be a session's shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub program: String,
    pub argv: Vec<String>,
}

/// A pane a multiplexer has just made.  Both ids come back because the two
/// answer different questions: `terminal_id` is the identity a session is
/// keyed on and survives the pane moving, `pane_id` is what an attach is
/// pointed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedPane {
    pub terminal_id: String,
    pub pane_id: String,
}

/// What a multiplexer answers so alacritree can host one of its panes.
#[enum_dispatch]
pub trait MultiplexerSession {
    /// The command that opens a session already showing `target`, when this
    /// multiplexer can hand one pane over on this side under `mode`.  `None`
    /// means the pane is reachable only by sharing the multiplexer's whole
    /// view, which `shared_view_gesture` prepares.
    fn open_multiplexer_session(&self, target: &PaneTarget, mode: AttachMode) -> Option<Launch>;

    /// What a shared view needs before its client can start: point the
    /// multiplexer at the pane, then name the session to attach to.  Both are
    /// process calls, so this only ever runs off the UI thread.
    ///
    /// `cached_name` is the session name already learned in the background.
    /// A gesture that beats the first read asks the multiplexer itself, since
    /// a wait is better than a refusal.
    fn shared_view_gesture(
        &self,
        target: &PaneTarget,
        cached_name: Option<String>,
    ) -> Result<Launch, String>;

    /// Open a pane on `side` and focus it, so a shared view attaching
    /// afterwards is already showing the pane this names.  A process call,
    /// so this only ever runs off the UI thread.
    ///
    /// `cwd` is spelled in the side's own terms: a Windows path on the native
    /// side, a path inside the distro on a WSL one, since the multiplexer
    /// resolves it where it runs.  `None` leaves the directory to the
    /// multiplexer's own default.
    fn create_pane(&self, side: &Side, cwd: Option<String>) -> Result<CreatedPane, String>;
}

/// herdr, reached through the free functions in [`crate::herdr`].
///
/// `Default` is what the enum's `FromStr` and `EnumIter` build the variant's
/// payload with.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Herdr;

impl MultiplexerSession for Herdr {
    fn open_multiplexer_session(&self, target: &PaneTarget, mode: AttachMode) -> Option<Launch> {
        if !herdr::attaches_directly(&target.side, mode, target.has_agent) {
            return None;
        }
        let args = herdr::attach_args(&target.pane_id);
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        let (program, argv) = target.side.command(herdr::PROGRAM, &borrowed);
        Some(Launch { program, argv })
    }

    fn shared_view_gesture(
        &self,
        target: &PaneTarget,
        cached_name: Option<String>,
    ) -> Result<Launch, String> {
        let focus = herdr::focus_args(target);
        let (program, argv) = herdr::herdr_attach_gesture(&target.side, &focus, cached_name)?;
        Ok(Launch { program, argv })
    }

    fn create_pane(&self, side: &Side, cwd: Option<String>) -> Result<CreatedPane, String> {
        herdr::create_pane(side, cwd)
    }
}

/// The terminal multiplexers alacritree can host a pane from.
#[enum_dispatch(MultiplexerSession)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Display, EnumString, EnumIter)]
#[strum(serialize_all = "lowercase")]
pub enum Multiplexer {
    Herdr(Herdr),
}

impl Multiplexer {
    /// The multiplexer that owns the pane `key` names.  Every key alacritree
    /// holds today names a herdr pane; a second multiplexer decides here, off
    /// whatever its own keys carry, and nowhere else.
    pub fn owning(_key: &herdr::HerdrKey) -> Self {
        Herdr.into()
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use strum::IntoEnumIterator;

    use super::*;

    fn pane(side: Side, has_agent: bool) -> PaneTarget {
        PaneTarget { side, pane_id: "w1:p1".into(), tab_id: Some("w1:t1".into()), has_agent }
    }

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
    fn native_runs_herdr_directly() {
        let (program, args) = Side::Native.command(herdr::PROGRAM, &["agent", "list"]);
        assert_eq!(program, "herdr");
        assert_eq!(args, vec!["agent", "list"]);
    }

    /// herdr installs to ~/.local/bin, which reaches PATH only under a login
    /// shell.  `wsl.exe -e herdr` fails with execvpe ENOENT.
    #[test]
    fn wsl_wraps_in_a_login_shell() {
        let (program, args) =
            Side::Wsl("kali-linux".into()).command(herdr::PROGRAM, &["agent", "list"]);
        assert_eq!(program, "wsl.exe");
        assert_eq!(args, vec!["-d", "kali-linux", "--exec", "sh", "-lc", "herdr agent list"]);
    }

    #[test]
    fn wsl_quotes_arguments_that_need_it() {
        let (_, args) =
            Side::Wsl("d".into()).command(herdr::PROGRAM, &["agent", "attach", "w1:p1"]);
        assert_eq!(args.last().unwrap(), "herdr agent attach 'w1:p1'");
    }

    /// A client reads a multiplexer's name off a reply and may send it back,
    /// so the two spellings have to agree.  `herdr` is lowercase because that
    /// is the string already on the wire.
    #[test]
    fn a_multiplexer_reads_back_as_the_name_it_spelled() {
        assert_eq!(Multiplexer::from(Herdr).to_string(), "herdr");
        for multiplexer in Multiplexer::iter() {
            assert_eq!(Multiplexer::from_str(&multiplexer.to_string()), Ok(multiplexer));
        }
    }

    /// A pane with an agent on a side that can hand one over opens directly,
    /// and the same pane with no agent does not, because every `herdr agent`
    /// subcommand resolves through a registry holding nothing for it.
    #[test]
    fn a_pane_with_no_agent_is_never_opened_directly() {
        let herdr = Multiplexer::from(Herdr);
        let wsl = Side::Wsl("d".into());
        let launch = herdr
            .open_multiplexer_session(&pane(wsl.clone(), true), AttachMode::Agent)
            .expect("a WSL pane with an agent hands the pane over");
        assert!(
            launch.argv.last().is_some_and(|script| script.ends_with("agent attach 'w1:p1'")),
            "{launch:?} does not attach the pane",
        );
        assert_eq!(herdr.open_multiplexer_session(&pane(wsl, false), AttachMode::Agent), None);
    }

    /// The configured attach mode outranks capability in one direction only:
    /// asking for a shared view always gets one, and no user is handed a
    /// direct attach they did not ask for.
    #[test]
    fn asking_for_a_shared_view_never_opens_a_pane_directly() {
        for side in [Side::Native, Side::Wsl("d".into())] {
            let target = pane(side, true);
            let opened =
                Multiplexer::from(Herdr).open_multiplexer_session(&target, AttachMode::Session);
            assert_eq!(opened, None, "{target:?} was handed over unasked");
        }
    }
}
