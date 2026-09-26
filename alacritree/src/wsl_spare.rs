//! Warm spare terminals for WSL, under `[wsl] warm_spare`.
//!
//! A `wsl.exe` launch can hang for half a minute while a fragmented WSL VM
//! compacts memory for its vmbus ring buffers.  This keeps one `wsl.exe` per
//! distro already past that point, parked in [`SPARE_SCRIPT`], and hands it
//! to the next session opening there, whose line names what to run.  Only a
//! distro a session already opened in gets one, so a spare never boots a VM.

use std::collections::HashMap;
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use alacritree_common::{wsl, wsl_helper};
use alacritty_terminal::event::WindowSize;
use alacritty_terminal::tty::{self, EventedPty, EventedReadWrite, Options as PtyOptions, Shell};

use crate::focus_priority::PriorityJob;
use crate::process_probe;
use crate::session::SESSION_ID_ENV;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Set once at startup from `[wsl] warm_spare`.
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Park until one line of shell-quoted words arrives, then run it: probe key,
/// session id, directory, argv, where no argv means the login shell, found
/// the way `wsl_helper::SHIM_SCRIPT` finds it.  Echo is off before the title
/// announces readiness, so the line never shows in the session.  The `\r` is
/// stripped by hand too, so a pipe reads the same line a terminal does.
pub(crate) const SPARE_SCRIPT: &str = r##"t=$(stty -g 2>/dev/null); stty -echo 2>/dev/null; printf '\033]2;%s\007' "$1"; IFS= read -r l || exit 1; [ -z "$t" ] || stty "$t"; cr=$(printf '\r'); eval "set -- ${l%"$cr"}"; k=$1; [ -z "$2" ] || export ALACRITREE_SESSION_ID="$2"; if [ -n "$3" ]; then cd "$3" || cd; else cd; fi; shift 3; [ -z "$k" ] || { d=${XDG_RUNTIME_DIR:-/tmp}/alacritree; mkdir -p "$d" 2>/dev/null && printf %s $$ > "$d/session-$k.pid"; }; [ $# -eq 0 ] || exec "$@"; s=$(getent passwd "$(id -un)" 2>/dev/null | cut -d: -f7); [ -x "$s" ] || s=/bin/sh; exec "$s" -l"##;

/// The title a spare sets once it is parked on its read.  ConPTY renders the
/// console rather than relaying bytes, so plain text could be coalesced away
/// before it reached the pipe, while a title change is always forwarded.
const READY_TITLE: &str = "alacritree-spare-ready";

/// Longer than the stalls this exists to hide, which run 25 to 30 s.
const READY_TIMEOUT: Duration = Duration::from_secs(90);

const READY_POLL: Duration = Duration::from_millis(20);

/// Under the 4095 bytes a Linux terminal holds for one unread line; anything
/// longer is cut off there and runs something other than what was asked.
const MAX_LINE: usize = 4000;

/// The size a spare parks at.  The session that takes it resizes it first.
const SPARE_SIZE: WindowSize =
    WindowSize { num_lines: 24, num_cols: 80, cell_width: 8, cell_height: 16 };

/// A WSL launch in the terms a spare can replay.  Only the argv shapes
/// alacritree builds itself parse, plus a bare `--exec`; anything else
/// launches cold, since a spare that ran something slightly different would
/// be worse than a slow tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Launch {
    distro: String,
    probe_key: Option<String>,
    /// The distro's own spelling.  Empty means the home directory.
    dir: String,
    /// Empty means the login shell.
    argv: Vec<String>,
}

impl Launch {
    /// `cwd` is the directory the PTY would start in, which is where wsl.exe
    /// starts too when the argv has no `--cd`.
    pub(crate) fn parse(program: &str, args: &[String], cwd: Option<&Path>) -> Option<Self> {
        if !wsl_helper::is_wsl_program(program) {
            return None;
        }
        let (flags, distro, rest) = wsl_helper::split_leading_flags(args)?;
        let distro =
            distro.or_else(|| wsl::distros().into_iter().find(|d| d.is_default).map(|d| d.name))?;
        let (probe_key, argv) = match rest {
            [] => (None, Vec::new()),
            [exec, sh, c, script, argv0, key]
                if exec == "--exec"
                    && sh == "sh"
                    && c == "-c"
                    && script == wsl_helper::SHIM_SCRIPT
                    && argv0 == "sh" =>
            {
                (Some(key.clone()), Vec::new())
            },
            [exec, sh, c, script, argv0, key, command @ ..]
                if exec == "--exec"
                    && sh == "sh"
                    && c == "-c"
                    && script == wsl_helper::EXEC_SHIM_SCRIPT
                    && argv0 == "sh"
                    && !command.is_empty() =>
            {
                (Some(key.clone()), command.to_vec())
            },
            [exec, command @ ..] if exec == "--exec" && !command.is_empty() => {
                (None, command.to_vec())
            },
            _ => return None,
        };
        let cd = flags.chunks(2).find(|pair| pair[0] == "--cd").map(|pair| pair[1].as_str());
        let dir = match cd {
            Some(cd) => distro_dir(cd, &distro)?,
            None => {
                let cwd = cwd.map(Path::to_path_buf).or_else(|| std::env::current_dir().ok())?;
                distro_dir(cwd.to_str()?, &distro)?
            },
        };
        Some(Self { distro, probe_key, dir, argv })
    }

    /// The line [`SPARE_SCRIPT`] reads, or `None` when a word cannot survive
    /// a terminal's line discipline.  `session_id` is empty when the id does
    /// not cross into the distro, so the spare exports what a cold launch
    /// would.
    fn line(&self, session_id: &str) -> Option<Vec<u8>> {
        let key = self.probe_key.as_deref().unwrap_or("");
        let words =
            [key, session_id, &self.dir].into_iter().chain(self.argv.iter().map(String::as_str));
        let mut line = String::new();
        for word in words {
            if word.chars().any(char::is_control) {
                return None;
            }
            line.push('\'');
            line.push_str(&word.replace('\'', r"'\''"));
            line.push_str("' ");
        }
        line.push('\r');
        (line.len() <= MAX_LINE).then(|| line.into_bytes())
    }
}

/// `--cd`'s argument, or wsl.exe's own translation of the Windows working
/// directory, as a path inside `distro`.  `None` is a path whose meaning this
/// cannot reproduce: `~/x`, or another distro's UNC path.
fn distro_dir(path: &str, distro: &str) -> Option<String> {
    if path == "~" {
        return Some(String::new());
    }
    if path.starts_with('/') {
        return Some(path.to_string());
    }
    match wsl::classify(Path::new(path)) {
        wsl::Location::Wsl { distro: owner, linux_path } => {
            owner.eq_ignore_ascii_case(distro).then_some(linux_path)
        },
        wsl::Location::Windows(path) => wsl::windows_to_linux(&path),
    }
}

/// What a spare's PTY was opened with.  A request asking for anything else
/// launches cold and the spare is replaced, which is how a config reload
/// reaches the spares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Template {
    env: HashMap<String, String>,
    boost: bool,
    reap: bool,
}

impl Template {
    /// The session id is left out: it differs per session, and the line
    /// carries it instead.
    pub(crate) fn new(env: &HashMap<String, String>, boost: bool, reap: bool) -> Self {
        let mut env = env.clone();
        env.remove(SESSION_ID_ENV);
        Self { env, boost, reap }
    }
}

/// A spare parked on its read.
struct Spare {
    pty: tty::Pty,
    shell_pid: Option<u32>,
    priority_job: Option<PriorityJob>,
    /// What the terminal printed before the ready title, minus that title,
    /// for the session to parse as if it had read it itself.
    preamble: Vec<u8>,
    template: Template,
}

/// A spare handed to a session, and the line that tells it what to run.
pub(crate) struct Claimed {
    pub pty: tty::Pty,
    pub shell_pid: Option<u32>,
    pub priority_job: Option<PriorityJob>,
    pub preamble: Vec<u8>,
    pub line: Vec<u8>,
}

/// Take the spare parked for `launch`'s distro, and launch its replacement.
/// `None` means launch cold: no spare is parked yet, it was opened with
/// another template, it died while parked, or the line cannot carry
/// `launch`.  A cold launch still starts a spare, so the next one is warm.
pub(crate) fn claim(launch: &Launch, session_id: &str, template: &Template) -> Option<Claimed> {
    let session_id = if wslenv_lists(SESSION_ID_ENV) { session_id } else { "" };
    let claimed = launch.line(session_id).and_then(|line| {
        let taken = pool().take(&launch.distro);
        let mut spare = taken.filter(|spare| spare.template == *template)?;
        if spare.pty.next_child_event().is_some() {
            log::debug!("the {} spare exited while parked", launch.distro);
            return None;
        }
        Some(Claimed {
            pty: spare.pty,
            shell_pid: spare.shell_pid,
            priority_job: spare.priority_job,
            preamble: spare.preamble,
            line,
        })
    });
    replenish(&launch.distro, template);
    claimed
}

/// Whether `WSLENV` carries `name` into the distro, which is what decides
/// whether a cold launch would see it there.
fn wslenv_lists(name: &str) -> bool {
    std::env::var("WSLENV")
        .is_ok_and(|wslenv| wslenv.split(':').any(|entry| entry.split('/').next() == Some(name)))
}

/// Drop every parked spare and stop launching new ones.  Statics are never
/// dropped, so exit has to call this or the parked `wsl.exe`s outlive the app.
pub fn shutdown() {
    let parked = pool().close();
    drop(parked);
}

fn replenish(distro: &str, template: &Template) {
    if !pool().reserve(distro) {
        return;
    }
    let distro = distro.to_string();
    let template = template.clone();
    let spawned = std::thread::Builder::new().name("wsl-spare".into()).spawn({
        let distro = distro.clone();
        move || {
            let started = Instant::now();
            match launch(&distro, template) {
                Ok(spare) => {
                    log::debug!("the {distro} spare is ready after {:?}", started.elapsed());
                    let rejected = pool().fill(&distro, spare).err();
                    drop(rejected);
                },
                Err(e) => {
                    log::warn!("the {distro} spare did not start: {e}");
                    pool().abandon(&distro);
                },
            }
        }
    });
    if let Err(e) = spawned {
        log::warn!("cannot start a thread for the {distro} spare: {e}");
        pool().abandon(&distro);
    }
}

/// Open the spare's PTY and wait for it to park.  The job is made here rather
/// than when a session takes the spare: a process joins a job when it is
/// created, so waiting would leave out whatever `wsl.exe` has started by then.
fn launch(distro: &str, template: Template) -> io::Result<Spare> {
    let args = ["-d", distro, "--exec", "sh", "-c", SPARE_SCRIPT, "sh", READY_TITLE];
    let options = PtyOptions {
        shell: Some(Shell::new(
            "wsl.exe".to_string(),
            args.iter().map(|arg| arg.to_string()).collect(),
        )),
        working_directory: None,
        drain_on_exit: true,
        env: template.env.clone(),
        #[cfg(windows)]
        escape_args: true,
    };

    #[cfg(windows)]
    crate::dll_search::harden_dll_search_path();

    let mut pty = tty::new(&options, SPARE_SIZE, 0)?;
    let shell_pid = process_probe::shell_pid_of(&pty);
    let priority_job = shell_pid
        .filter(|_| template.boost || template.reap)
        .and_then(|pid| PriorityJob::adopt(pid, template.reap));
    let preamble = await_ready(&mut pty)?;
    Ok(Spare { pty, shell_pid, priority_job, preamble, template })
}

fn await_ready(pty: &mut tty::Pty) -> io::Result<Vec<u8>> {
    let deadline = Instant::now() + READY_TIMEOUT;
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let read = match pty.reader().read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => 0,
            Err(e) => return Err(e),
        };
        seen.extend_from_slice(&buf[..read]);
        if let Some(preamble) = without_ready_title(&seen) {
            return Ok(preamble);
        }
        if pty.next_child_event().is_some() {
            return Err(io::Error::other("wsl.exe exited before the spare was ready"));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "the spare never became ready"));
        }
        if read == 0 {
            std::thread::sleep(READY_POLL);
        }
    }
}

/// `seen` with the ready title's OSC cut out, once the whole sequence has
/// arrived.  ConPTY may re-spell the title as OSC 0, so only the text is
/// matched, and the sequence is whatever `ESC ]` precedes it up to the BEL.
fn without_ready_title(seen: &[u8]) -> Option<Vec<u8>> {
    let title = READY_TITLE.as_bytes();
    let at = seen.windows(title.len()).position(|w| w == title)?;
    let start = seen[..at].windows(2).rposition(|w| w == b"\x1b]")?;
    let end = at + title.len() + seen[at + title.len()..].iter().position(|&b| b == b'\x07')?;
    let mut preamble = seen[..start].to_vec();
    preamble.extend_from_slice(&seen[end + 1..]);
    Some(preamble)
}

/// A launch into `distro` whose spare is forever launching: it is never
/// ready to take, and claiming it starts no replacement.
#[cfg(all(test, windows))]
pub(crate) fn launch_with_no_spare_ready(distro: &str) -> Launch {
    assert!(pool().reserve(distro), "the distro is this test's alone");
    Launch { distro: distro.to_string(), probe_key: None, dir: String::new(), argv: Vec::new() }
}

fn pool() -> MutexGuard<'static, Pool<Spare>> {
    static POOL: Mutex<Pool<Spare>> = Mutex::new(Pool { slots: None, closed: false });
    POOL.lock().unwrap_or_else(PoisonError::into_inner)
}

enum Slot<S> {
    Launching,
    Ready(S),
}

/// At most one spare per distro, parked or on its way.  Generic so its rules
/// can be tested without launching anything.  Spares leave through return
/// values rather than being dropped here, because closing a PTY can block and
/// this is only ever used under a lock.
struct Pool<S> {
    /// `None` until first used, so the pool can be a `static`.
    slots: Option<HashMap<String, Slot<S>>>,
    closed: bool,
}

impl<S> Pool<S> {
    fn slots(&mut self) -> &mut HashMap<String, Slot<S>> {
        self.slots.get_or_insert_with(HashMap::new)
    }

    /// The parked spare, if one is.  A spare still launching stays put.
    fn take(&mut self, distro: &str) -> Option<S> {
        let slots = self.slots();
        match slots.get(distro) {
            Some(Slot::Ready(_)) => match slots.remove(distro) {
                Some(Slot::Ready(spare)) => Some(spare),
                _ => unreachable!("the slot was just seen ready"),
            },
            _ => None,
        }
    }

    /// Claim the right to launch `distro`'s spare.  `false` when one is
    /// already parked or launching, or the pool is closed.
    fn reserve(&mut self, distro: &str) -> bool {
        if self.closed || self.slots().contains_key(distro) {
            return false;
        }
        self.slots().insert(distro.to_string(), Slot::Launching);
        true
    }

    /// Park a spare whose launch [`Pool::reserve`] granted.  A pool closed
    /// meanwhile has no slot for it and hands it back to be dropped.
    fn fill(&mut self, distro: &str, spare: S) -> Result<(), S> {
        match self.slots().get_mut(distro) {
            Some(slot @ Slot::Launching) => {
                *slot = Slot::Ready(spare);
                Ok(())
            },
            _ => Err(spare),
        }
    }

    fn abandon(&mut self, distro: &str) {
        if matches!(self.slots().get(distro), Some(Slot::Launching)) {
            self.slots().remove(distro);
        }
    }

    fn close(&mut self) -> Vec<S> {
        self.closed = true;
        self.slots()
            .drain()
            .filter_map(|(_, slot)| match slot {
                Slot::Ready(spare) => Some(spare),
                Slot::Launching => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    fn parse(args: &[String]) -> Option<Launch> {
        Launch::parse("wsl.exe", args, None)
    }

    #[test]
    fn a_shimmed_login_shell_parses_to_its_key_and_directory() {
        let (program, args) =
            wsl_helper::shim_invocation("Ubuntu", Path::new("/home/me/src"), "k1");
        let launch = Launch::parse(&program, &args, None).expect("a shim parses");
        assert_eq!(launch, Launch {
            distro: "Ubuntu".into(),
            probe_key: Some("k1".into()),
            dir: "/home/me/src".into(),
            argv: vec![],
        });
    }

    #[test]
    fn a_shimmed_attach_parses_to_its_command() {
        let args = strings(&["-d", "Ubuntu", "--cd", "/w", "--exec", "herdr", "attach"]);
        let wrapped = wsl_helper::wrap_exec_argv("wsl.exe", &args, "k2").expect("wraps");
        let launch = parse(&wrapped).expect("a wrapped attach parses");
        assert_eq!(launch.probe_key.as_deref(), Some("k2"));
        assert_eq!(launch.argv, strings(&["herdr", "attach"]));
        assert_eq!(launch.dir, "/w");
    }

    #[test]
    fn an_unshimmed_launch_parses_without_a_key() {
        let (_, args) = wsl::shell_invocation("Ubuntu", Path::new("/home/me"));
        assert_eq!(parse(&args).expect("parses").probe_key, None);

        let (_, args) = wsl::exec_invocation("Ubuntu", &["sh", "-lc", "exec zellij attach x"]);
        let launch = Launch::parse("wsl.exe", &args, Some(Path::new("/srv"))).expect("parses");
        assert_eq!(launch.argv, strings(&["sh", "-lc", "exec zellij attach x"]));
        assert_eq!(launch.dir, "/srv");
    }

    #[test]
    fn a_launch_this_cannot_replay_stays_cold() {
        assert_eq!(parse(&strings(&["-d", "Ubuntu", "--user", "root"])), None);
        assert_eq!(parse(&strings(&["-d", "Ubuntu", "--cd", "~/src"])), None);
        assert_eq!(
            parse(&strings(&["-d", "Ubuntu", "--cd", r"\\wsl.localhost\Debian\home"])),
            None
        );
        assert_eq!(Launch::parse("pwsh.exe", &strings(&["-d", "Ubuntu"]), None), None);
    }

    #[cfg(windows)]
    #[test]
    fn a_windows_directory_is_spelled_the_way_the_distro_sees_it() {
        let args = strings(&["-d", "Ubuntu", "--cd", r"C:\Users\me"]);
        assert_eq!(parse(&args).expect("parses").dir, "/mnt/c/Users/me");
        let args = strings(&["-d", "Ubuntu", "--cd", r"\\wsl.localhost\Ubuntu\home\me"]);
        assert_eq!(parse(&args).expect("parses").dir, "/home/me");
        let args = strings(&["-d", "Ubuntu", "--cd", "~"]);
        assert_eq!(parse(&args).expect("parses").dir, "");
    }

    #[test]
    fn the_line_quotes_every_word_and_refuses_control_characters() {
        let launch = Launch {
            distro: "Ubuntu".into(),
            probe_key: None,
            dir: "/it's here".into(),
            argv: strings(&["echo", "a b"]),
        };
        assert_eq!(
            launch.line("7").expect("encodes"),
            br#"'' '7' '/it'\''s here' 'echo' 'a b' "#.iter().chain(b"\r").copied().collect::<Vec<_>>()
        );

        let tab = Launch { argv: strings(&["printf", "a\tb"]), ..launch.clone() };
        assert_eq!(tab.line("7"), None);
        let long = Launch { argv: vec!["x".repeat(MAX_LINE)], ..launch };
        assert_eq!(long.line("7"), None);
    }

    #[test]
    fn the_script_exports_the_variable_sessions_are_named_by() {
        assert!(SPARE_SCRIPT.contains(&format!("export {SESSION_ID_ENV}=")));
    }

    #[test]
    fn the_ready_title_is_cut_out_once_it_has_fully_arrived() {
        let partial = b"\x1b[?25l\x1b]0;alacritree-spare-ready";
        assert_eq!(without_ready_title(partial), None);

        let whole = b"\x1b[?25l\x1b]0;alacritree-spare-ready\x07\x1b[?25h";
        assert_eq!(without_ready_title(whole).expect("ready"), b"\x1b[?25l\x1b[?25h");
    }

    #[test]
    fn a_launch_with_no_parked_spare_launches_cold() {
        let mut pool = Pool::<u32> { slots: None, closed: false };
        assert_eq!(pool.take("Ubuntu"), None, "nothing was ever launched");

        assert!(pool.reserve("Ubuntu"));
        assert!(!pool.reserve("Ubuntu"), "one spare per distro");
        assert_eq!(pool.take("Ubuntu"), None, "a spare still launching is not ready");

        assert_eq!(pool.fill("Ubuntu", 1), Ok(()));
        assert_eq!(pool.take("Ubuntu"), Some(1));
        assert_eq!(pool.take("Ubuntu"), None, "a spare is taken once");
    }

    #[test]
    fn a_closed_pool_hands_back_what_it_held_and_takes_nothing_new() {
        let mut pool = Pool::<u32> { slots: None, closed: false };
        assert!(pool.reserve("Ubuntu"));
        assert_eq!(pool.fill("Ubuntu", 1), Ok(()));
        assert!(pool.reserve("Debian"));

        assert_eq!(pool.close(), vec![1]);
        assert_eq!(pool.fill("Debian", 2), Err(2));
        assert!(!pool.reserve("Ubuntu"));
    }

    /// The script against a pipe instead of a terminal: `stty` fails and is
    /// skipped, and everything the line decides still happens.
    #[cfg(unix)]
    #[test]
    fn the_spare_script_runs_exactly_the_line_it_is_given() {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let runtime = tempfile::tempdir().expect("a runtime dir");
        let workdir = runtime.path().join("work dir");
        std::fs::create_dir(&workdir).expect("a working dir");
        let launch = Launch {
            distro: "Ubuntu".into(),
            probe_key: Some("k1".into()),
            dir: workdir.to_str().expect("utf-8").to_string(),
            argv: strings(&[
                "sh",
                "-c",
                r#"printf '%s|' "$PWD" "$ALACRITREE_SESSION_ID" "$$" "$1""#,
                "sh",
                "it's two words",
            ]),
        };

        #[allow(clippy::disallowed_methods)] // A test running the script is the point.
        let mut child = Command::new("sh")
            .args(["-c", SPARE_SCRIPT, "sh", READY_TITLE])
            .env("XDG_RUNTIME_DIR", runtime.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("sh runs");
        // A terminal's `icrnl` ends the line; a pipe passes the `\r` through.
        let mut line = launch.line("7").expect("encodes");
        line.push(b'\n');
        child.stdin.take().expect("stdin").write_all(&line).unwrap();
        let output = child.wait_with_output().expect("the script exits");

        let stdout = String::from_utf8(output.stdout).expect("utf-8");
        let ran = stdout
            .strip_prefix(&format!("\x1b]2;{READY_TITLE}\x07"))
            .expect("the script announces itself first");
        let pid = std::fs::read_to_string(runtime.path().join("alacritree/session-k1.pid"))
            .expect("the pid file");
        assert_eq!(ran, format!("{}|7|{pid}|it's two words|", workdir.display()));
    }
}
