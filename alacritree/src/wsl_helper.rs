//! Resident WSL helper: one long-lived `sh` per distro, spoken to over its
//! stdio pipe, serving the batch scripts (`RUN`), the foreground-process
//! probe (`PROBE`), and tool paths (the hello line) without a per-call
//! `wsl.exe` spawn.  The wire protocol is the seam a future compiled helper
//! would slot behind; nothing outside this module knows it exists.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;

/// Bumped only when the request/response framing changes incompatibly; a
/// client seeing any other version treats the helper as unusable and stays
/// on one-shot spawns.
pub const PROTOCOL_VERSION: &str = "1";

/// Login-shell-resolved tool paths and the distro-side runtime dir, from
/// the helper's hello line.  `None` means the tool wasn't on the login
/// shell's PATH at helper start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub git: Option<String>,
    pub delta: Option<String>,
    pub gh: Option<String>,
    pub runtime_dir: String,
}

/// A request-side payload field: base64, or `-` for the empty payload.
/// Tab is IFS whitespace in sh, so an empty field would be collapsed away
/// by the dispatcher's field splitting; base64 can never produce a bare
/// `-`, so the encodings stay disjoint.
fn encode_field(payload: &str) -> String {
    if payload.is_empty() { "-".to_string() } else { B64.encode(payload) }
}

pub fn encode_run(id: u64, script: &str, args: &[&str]) -> String {
    let mut line = format!("{id}\tRUN\t{}", encode_field(script));
    for arg in args {
        line.push('\t');
        line.push_str(&encode_field(arg));
    }
    line.push('\n');
    line
}

pub fn encode_probe(id: u64, key: &str) -> String {
    format!("{id}\tPROBE\t{key}\n")
}

pub fn parse_hello(line: &str) -> Option<Capabilities> {
    // Strip only line terminators — trim_end() would also eat the tab
    // before a legitimately empty trailing field.
    let mut fields = line.trim_end_matches(['\r', '\n']).split('\t');
    if fields.next()? != "hello" || fields.next()? != PROTOCOL_VERSION {
        return None;
    }
    let mut decode = || -> Option<String> {
        let raw = B64.decode(fields.next()?).ok()?;
        Some(String::from_utf8_lossy(&raw).trim().to_string())
    };
    let git = decode()?;
    let delta = decode()?;
    let gh = decode()?;
    let runtime_dir = decode()?;
    let some = |s: String| (!s.is_empty()).then_some(s);
    Some(Capabilities { git: some(git), delta: some(delta), gh: some(gh), runtime_dir })
}

/// One response off the helper's stdout: `<id>\t<exit>\t<len>\n` followed
/// by exactly `len` raw payload bytes.
#[derive(Debug, PartialEq, Eq)]
pub struct Frame {
    pub id: u64,
    pub exit: i32,
    pub payload: Vec<u8>,
}

/// Incremental response parser fed arbitrary read chunks; complete frames
/// come out as they close.  A malformed header is unrecoverable (the byte
/// count is the only framing, so there is no resync point) and surfaces as
/// an error for the caller to tear the client down on.
#[derive(Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, String> {
        self.buf.extend_from_slice(bytes);
        let mut frames = Vec::new();
        loop {
            let Some(newline) = self.buf.iter().position(|&b| b == b'\n') else {
                return Ok(frames);
            };
            let Some((id, exit, len)) = parse_header(&self.buf[..newline]) else {
                return Err(format!(
                    "malformed helper frame header: {:?}",
                    String::from_utf8_lossy(&self.buf[..newline])
                ));
            };
            let Some(payload_start) = newline.checked_add(1) else {
                return Err(format!(
                    "malformed helper frame header: {:?}",
                    String::from_utf8_lossy(&self.buf[..newline])
                ));
            };
            let Some(frame_end) = payload_start.checked_add(len) else {
                return Err(format!(
                    "malformed helper frame header: {:?}",
                    String::from_utf8_lossy(&self.buf[..newline])
                ));
            };
            if self.buf.len() < frame_end {
                return Ok(frames);
            }
            frames.push(Frame { id, exit, payload: self.buf[payload_start..frame_end].to_vec() });
            self.buf.drain(..frame_end);
        }
    }
}

fn parse_header(line: &[u8]) -> Option<(u64, i32, usize)> {
    let text = std::str::from_utf8(line).ok()?;
    let mut fields = text.trim_end_matches('\r').split('\t');
    // `wc -c` output may carry leading blanks on some implementations.
    let id = fields.next()?.trim().parse().ok()?;
    let exit = fields.next()?.trim().parse().ok()?;
    let len = fields.next()?.trim().parse().ok()?;
    fields.next().is_none().then_some((id, exit, len))
}

use std::path::Path;

/// The distro-side helper, passed verbatim as the single argument of
/// `wsl.exe --exec sh -c`.  POSIX sh only — dash and busybox ash both run
/// it.  Shape: capability hello, dead-pidfile GC, a background writer that
/// owns stdout, then the request dispatcher on stdin.  Responses all leave
/// through the writer, whose FIFO completion lines are far under PIPE_BUF,
/// so concurrent jobs never interleave frames.  Commentary lives here, not
/// in the script, so every byte shipped into the distro earns its keep.
///
/// Empty request fields arrive as `-` (see `encode_field`); decoded args
/// lose trailing newlines to command substitution, which no current caller
/// passes.  Stdin EOF ends the dispatcher; the EXIT trap removes the temp
/// dir and `kill 0` takes the writer and any in-flight jobs down with the
/// process group, so a job can never deadlock on the deleted FIFO.
pub(crate) const HELPER_SCRIPT: &str = r##"
set -u
b64() { printf %s "$1" | base64 | tr -d '\n'; }
s=$(getent passwd "$(id -un)" 2>/dev/null | cut -d: -f7)
[ -x "$s" ] || s=${SHELL:-/bin/sh}
caps=$("$s" -lc 'command -v git || echo; command -v delta || echo; command -v gh || echo' 2>/dev/null)
rt=${XDG_RUNTIME_DIR:-/tmp}/alacritree
printf 'hello\t1\t%s\t%s\t%s\t%s\n' \
  "$(b64 "$(printf %s "$caps" | sed -n 1p)")" \
  "$(b64 "$(printf %s "$caps" | sed -n 2p)")" \
  "$(b64 "$(printf %s "$caps" | sed -n 3p)")" \
  "$(b64 "$rt")"
mkdir -p "$rt" 2>/dev/null
for f in "$rt"/session-*.pid; do
  [ -e "$f" ] || continue
  p=$(cat "$f" 2>/dev/null)
  case $p in ''|*[!0-9]*) rm -f "$f"; continue;; esac
  [ -d "/proc/$p" ] || rm -f "$f"
done
t=$(mktemp -d) || exit 1
mkfifo "$t/done" || exit 1
trap 'rm -rf "$t"; kill 0 2>/dev/null' EXIT
(
  exec 3<>"$t/done"
  while read -r id code <&3; do
    out="$t/$id.out"
    n=$(wc -c < "$out" 2>/dev/null) || n=0
    printf '%s\t%s\t%s\n' "$id" "$code" "${n:-0}"
    cat "$out" 2>/dev/null
    rm -f "$out"
  done
) &
TAB=$(printf '\t')
while IFS=$TAB read -r id kind rest; do
  case $kind in
  RUN)
    (
      script=
      set --
      first=1
      line=$rest
      while [ -n "$line" ]; do
        case $line in
        *"$TAB"*) field=${line%%"$TAB"*}; line=${line#*"$TAB"} ;;
        *) field=$line; line= ;;
        esac
        if [ "$field" = - ]; then dec=; else dec=$(printf %s "$field" | base64 -d 2>/dev/null); fi
        if [ "$first" = 1 ]; then script=$dec; first=0; else set -- "$@" "$dec"; fi
      done
      sh -c "$script" sh "$@" > "$t/$id.out" 2>/dev/null
      printf '%s %s\n' "$id" "$?" >> "$t/done"
    ) &
    ;;
  PROBE)
    comm=
    p=$(cat "$rt/session-$rest.pid" 2>/dev/null)
    case $p in ''|*[!0-9]*) p= ;; esac
    if [ -n "$p" ] && [ -d "/proc/$p" ]; then
      stat=$(cat "/proc/$p/stat" 2>/dev/null)
      after=${stat##*')'}
      set -- $after
      pgrp=${3:-}
      tpgid=${6:-}
      case $tpgid in ''|*[!0-9]*) tpgid= ;; esac
      if [ -n "$tpgid" ] && [ "$tpgid" != "$pgrp" ]; then
        comm=$(cat "/proc/$tpgid/comm" 2>/dev/null)
      fi
    fi
    printf %s "$comm" > "$t/$id.out"
    printf '%s 0\n' "$id" >> "$t/done"
    ;;
  esac
done
"##;

/// Login-shell shim for shimmed WSL sessions: publish the shell's PID under
/// the probe key, then become the user's login shell.  `exec` makes the
/// pidfile PID *be* the shell, so the helper's tpgid walk starts from the
/// right place.  wsl.exe's own no-`--exec` launch would start the login
/// shell too but gives no way to learn its PID; re-resolving through
/// `getent` is the documented divergence, with `/bin/sh` only as a last
/// resort.  Single line: it travels through ConPTY command-line quoting.
pub(crate) const SHIM_SCRIPT: &str = r##"d=${XDG_RUNTIME_DIR:-/tmp}/alacritree; mkdir -p "$d" 2>/dev/null && printf %s $$ > "$d/session-$1.pid"; s=$(getent passwd "$(id -un)" 2>/dev/null | cut -d: -f7); [ -x "$s" ] || s=/bin/sh; exec "$s" -l"##;

/// argv for a session alacritree constructs itself (`ShellChoice::Wsl`,
/// auto-by-location): the shim with the probe key as `$1`.
pub fn shim_invocation(distro: &str, workdir: &Path, probe_key: &str) -> (String, Vec<String>) {
    (
        "wsl.exe".to_string(),
        vec![
            "-d".to_string(),
            distro.to_string(),
            "--cd".to_string(),
            workdir.to_string_lossy().into_owned(),
            "--exec".to_string(),
            "sh".to_string(),
            "-c".to_string(),
            SHIM_SCRIPT.to_string(),
            "sh".to_string(),
            probe_key.to_string(),
        ],
    )
}

/// Probe-key shim for a `[[ui.profiles]]` entry that launches wsl.exe.
/// Only argv this parser fully understands is wrapped: any mix of
/// `-d`/`--distribution <distro>` and `--cd <dir>`, nothing else.  An
/// unknown flag or a positional command may not be a plain login shell —
/// it runs unmodified and simply probes as unknown.  Returns the rewritten
/// argv plus the explicit distro (`None` = the default distro; the caller
/// resolves it, since only `wsl::distros` knows which that is).
pub fn wrap_profile_argv(
    program: &str,
    args: &[String],
    probe_key: &str,
) -> Option<(Vec<String>, Option<String>)> {
    // The argv comes from a Windows host, so the program path uses Windows
    // separators — split on them explicitly rather than via `Path`, whose
    // separator set depends on the compilation target.
    let file_name = program.rsplit(['\\', '/']).next().unwrap_or(program);
    let stem = Path::new(file_name).file_stem()?.to_str()?;
    if !stem.eq_ignore_ascii_case("wsl") {
        return None;
    }
    let mut distro = None;
    let mut wrapped = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-d" | "--distribution" => {
                let name = it.next()?;
                distro = Some(name.clone());
                wrapped.push(arg.clone());
                wrapped.push(name.clone());
            },
            "--cd" => {
                let dir = it.next()?;
                wrapped.push(arg.clone());
                wrapped.push(dir.clone());
            },
            _ => return None,
        }
    }
    wrapped.extend([
        "--exec".to_string(),
        "sh".to_string(),
        "-c".to_string(),
        SHIM_SCRIPT.to_string(),
        "sh".to_string(),
        probe_key.to_string(),
    ]);
    Some((wrapped, distro))
}

use std::collections::HashMap;
use std::io::{BufRead, Read, Write};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, mpsc};
use std::time::{Duration, Instant};

use crate::wsl;

/// Batch scripts can legitimately run long (worktree add on a cold cache);
/// probes are two `/proc` reads and only ever gate a keypress decision.
const RUN_TIMEOUT: Duration = Duration::from_secs(60);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// A broken distro must not cause a spawn storm.
const RESPAWN_COOLDOWN: Duration = Duration::from_secs(30);

static ENABLED: AtomicBool = AtomicBool::new(true);

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Release);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

/// Why a request produced no result — the distinction the fallback rule
/// keys on.  `NotWritten` never reached the helper and is safe to re-run
/// as a one-shot; `NoReply` was written and may have executed (batch
/// scripts have side effects), so it must surface as an error, never a
/// silent retry.
#[derive(Debug)]
pub enum TransportError {
    NotWritten(String),
    NoReply(String),
}

pub struct HelperClient {
    distro: String,
    stdin: Mutex<Option<std::process::ChildStdin>>,
    pending: Mutex<HashMap<u64, mpsc::Sender<Frame>>>,
    next_id: AtomicU64,
    capabilities: OnceLock<Capabilities>,
    down: AtomicBool,
}

fn lock<'a, T>(mutex: &'a Mutex<T>) -> std::sync::MutexGuard<'a, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl HelperClient {
    /// Spawn the helper for `distro`.  Returns once the process launch is
    /// attempted; readiness (the hello line) arrives asynchronously on the
    /// reader thread.  Failures leave the client marked down so the
    /// registry's cooldown sees them like any other death.
    fn spawn(distro: &str) -> Arc<Self> {
        let client = Arc::new(Self {
            distro: distro.to_string(),
            stdin: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            capabilities: OnceLock::new(),
            down: AtomicBool::new(false),
        });
        let mut child = match wsl::command(distro, None)
            .arg("sh")
            .arg("-c")
            .arg(HELPER_SCRIPT)
            .arg("sh")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                client.mark_down(&format!("failed to spawn: {e}"));
                return client;
            },
        };
        *lock(&client.stdin) = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout piped above");
        let reader = client.clone();
        let spawned =
            std::thread::Builder::new().name(format!("wsl-helper-{distro}")).spawn(move || {
                reader.read_loop(stdout);
                // Stdin is closed by mark_down; reap so a dead helper never
                // lingers as a zombie in the process table.
                let _ = child.wait();
            });
        if let Err(e) = spawned {
            client.mark_down(&format!("failed to start reader thread: {e}"));
        }
        client
    }

    fn read_loop(&self, stdout: std::process::ChildStdout) {
        let mut reader = std::io::BufReader::new(stdout);
        let mut hello = String::new();
        match reader.read_line(&mut hello) {
            Ok(n) if n > 0 => {},
            _ => return self.mark_down("exited before hello"),
        }
        let Some(caps) = parse_hello(&hello) else {
            return self.mark_down("unusable hello (unknown protocol version?)");
        };
        let _ = self.capabilities.set(caps);
        let mut frames = FrameReader::default();
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => return self.mark_down("closed its pipe"),
                Err(e) => return self.mark_down(&format!("read failed: {e}")),
                Ok(n) => match frames.push(&chunk[..n]) {
                    Ok(done) => {
                        for frame in done {
                            if let Some(tx) = lock(&self.pending).remove(&frame.id) {
                                let _ = tx.send(frame);
                            }
                        }
                    },
                    Err(e) => return self.mark_down(&e),
                },
            }
        }
    }

    fn mark_down(&self, why: &str) {
        if !self.down.swap(true, Ordering::AcqRel) {
            log::warn!("wsl helper for {}: {why}; falling back to one-shot spawns", self.distro);
        }
        // Closing stdin EOFs the helper, which cleans up and exits.
        *lock(&self.stdin) = None;
        // Waiters whose request was already written see the hangup as a
        // dropped sender — NoReply, never a retry.
        lock(&self.pending).clear();
    }

    fn is_down(&self) -> bool {
        self.down.load(Ordering::Acquire)
    }

    fn is_ready(&self) -> bool {
        !self.is_down() && self.capabilities.get().is_some()
    }

    pub fn capabilities(&self) -> Option<&Capabilities> {
        self.capabilities.get()
    }

    fn request(&self, id: u64, line: String, timeout: Duration) -> Result<Frame, TransportError> {
        if !self.is_ready() {
            return Err(TransportError::NotWritten("helper not ready".to_string()));
        }
        let (tx, rx) = mpsc::channel();
        lock(&self.pending).insert(id, tx);
        let write = {
            let mut guard = lock(&self.stdin);
            match guard.as_mut() {
                None => Err("helper stdin closed".to_string()),
                Some(stdin) => stdin
                    .write_all(line.as_bytes())
                    .and_then(|()| stdin.flush())
                    .map_err(|e| e.to_string()),
            }
        };
        if let Err(e) = write {
            lock(&self.pending).remove(&id);
            // A partial line has no terminating newline, so the dispatcher
            // can never have run it — NotWritten is safe.
            self.mark_down(&format!("write failed: {e}"));
            return Err(TransportError::NotWritten(e));
        }
        match rx.recv_timeout(timeout) {
            Ok(frame) => Ok(frame),
            Err(_) => {
                lock(&self.pending).remove(&id);
                Err(TransportError::NoReply(format!("no reply from the {} helper", self.distro)))
            },
        }
    }

    pub fn run(&self, script: &str, args: &[&str]) -> Result<(i32, Vec<u8>), TransportError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let frame = self.request(id, encode_run(id, script, args), RUN_TIMEOUT)?;
        Ok((frame.exit, frame.payload))
    }

    pub fn probe(&self, key: &str) -> Result<Option<String>, TransportError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let frame = self.request(id, encode_probe(id, key), PROBE_TIMEOUT)?;
        let comm = String::from_utf8_lossy(&frame.payload).trim().to_string();
        Ok((!comm.is_empty()).then_some(comm))
    }
}

enum Slot {
    Live(Arc<HelperClient>),
    Cooldown(Instant),
}

fn registry() -> &'static Mutex<HashMap<String, Slot>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Slot>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The ready client for `distro`, spawning one when none exists.  `None`
/// while disabled, still starting, or cooling down after a death — callers
/// fall back to one-shot spawns, which pay the same cold-boot cost the
/// helper would.  Spawning happens under the registry lock but is only a
/// process launch; the slow part (the hello) lands on the reader thread.
/// Never call on the UI thread — same rule as `wsl::run_batch`.
pub fn client(distro: &str) -> Option<Arc<HelperClient>> {
    if !enabled() || !cfg!(windows) {
        return None;
    }
    let mut reg = lock(registry());
    match reg.get(distro) {
        Some(Slot::Live(c)) if c.is_ready() => return Some(c.clone()),
        Some(Slot::Live(c)) if !c.is_down() => return None,
        Some(Slot::Live(_)) => {
            reg.insert(distro.to_string(), Slot::Cooldown(Instant::now()));
            return None;
        },
        Some(Slot::Cooldown(since)) if since.elapsed() < RESPAWN_COOLDOWN => return None,
        _ => {},
    }
    reg.insert(distro.to_string(), Slot::Live(HelperClient::spawn(distro)));
    None
}

/// Resident-first transport for `wsl::run_batch`.  `None` = helper
/// unavailable before anything was sent (fall back to a one-shot spawn);
/// `Some(Err)` = sent but unanswered, which must not be retried;
/// `Some(Ok)` = script stdout, one-shot-compatible.
pub fn try_run(distro: &str, script: &str, args: &[&str]) -> Option<Result<Vec<u8>, String>> {
    let client = client(distro)?;
    match client.run(script, args) {
        Ok((exit, stdout)) => {
            // Mirror one-shot semantics: guarded scripts always emit their
            // sections, so hard failure with silence means the script
            // itself refused.
            if exit != 0 && stdout.is_empty() {
                Some(Err(format!("wsl helper script exited {exit}")))
            } else {
                Some(Ok(stdout))
            }
        },
        Err(TransportError::NotWritten(e)) => {
            log::debug!("wsl helper ({distro}): {e}; falling back to one-shot spawns");
            None
        },
        Err(TransportError::NoReply(e)) => Some(Err(e)),
    }
}

pub fn capability_delta(distro: &str) -> Option<String> {
    client(distro)?.capabilities()?.delta.clone()
}

pub fn capability_gh(distro: &str) -> Option<String> {
    client(distro)?.capabilities()?.gh.clone()
}

/// Identity of a shimmed WSL session for the foreground probe.
#[derive(Debug, Clone)]
pub struct WslProbe {
    pub distro: String,
    pub key: String,
}

const PROBE_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Last-known foreground `comm` per registered `(distro, probe key)`.
/// Written only by the poller thread (and tests); read from the UI thread.
fn probe_cache() -> &'static Mutex<HashMap<(String, String), Option<String>>> {
    static CACHE: OnceLock<Mutex<HashMap<(String, String), Option<String>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A probe key unique across alacritree instances: the pidfile dir inside
/// each distro is shared, so the Windows pid namespaces the per-instance
/// counter.
pub fn new_probe_key() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}

pub fn register_probe(distro: &str, key: &str) {
    lock(probe_cache()).insert((distro.to_string(), key.to_string()), None);
    ensure_poller();
}

pub fn unregister_probe(distro: &str, key: &str) {
    lock(probe_cache()).remove(&(distro.to_string(), key.to_string()));
}

/// Cached foreground `comm` for a shimmed WSL session — never blocks and
/// never touches the pipe, so it is safe on the UI thread.  `None` means
/// unknown (helper down, key unregistered, or an idle shell at the last
/// poll); callers must treat unknown as "no TUI".
pub fn foreground_comm(distro: &str, key: &str) -> Option<String> {
    lock(probe_cache()).get(&(distro.to_string(), key.to_string()))?.clone()
}

#[cfg(test)]
fn set_cached_comm(distro: &str, key: &str, comm: Option<String>) {
    lock(probe_cache()).insert((distro.to_string(), key.to_string()), comm);
}

/// One process-wide poller refreshes every registered key at the agent
/// cadence.  Requests leave this thread, so a slow helper delays freshness,
/// never the UI.  Polling a distro also (re)spawns its helper through
/// `client()`, so an open WSL session keeps nudging a cooled-down helper
/// back up.  The key list is snapshotted before any pipe I/O so the cache
/// lock is never held across a request.
fn ensure_poller() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let spawned =
            std::thread::Builder::new().name("wsl-helper-probe".to_string()).spawn(|| {
                loop {
                    std::thread::sleep(PROBE_POLL_INTERVAL);
                    let keys: Vec<(String, String)> = lock(probe_cache()).keys().cloned().collect();
                    for entry in keys {
                        let comm = client(&entry.0).and_then(|c| c.probe(&entry.1).ok()).flatten();
                        if let Some(slot) = lock(probe_cache()).get_mut(&entry) {
                            *slot = comm;
                        }
                    }
                }
            });
        if let Err(e) = spawned {
            log::warn!("wsl probe poller failed to start: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_request_encodes_base64_fields() {
        let line = encode_run(7, r#"printf %s "$1""#, &["hello"]);
        assert_eq!(line, "7\tRUN\tcHJpbnRmICVzICIkMSI=\taGVsbG8=\n");
    }

    #[test]
    fn empty_arg_encodes_as_dash() {
        // Tab is IFS whitespace in sh, so an empty field would be collapsed
        // away by the dispatcher's field splitting.
        let line = encode_run(1, "s", &["", "x"]);
        assert_eq!(line, "1\tRUN\tcw==\t-\teA==\n");
    }

    #[test]
    fn probe_request_is_plain() {
        assert_eq!(encode_probe(3, "1234-7"), "3\tPROBE\t1234-7\n");
    }

    #[test]
    fn parses_hello_with_missing_tools() {
        // git and runtime dir present, delta and gh absent (empty fields).
        let line = "hello\t1\tL3Vzci9iaW4vZ2l0\t\t\tL3J1bi91c2VyLzEwMDAvYWxhY3JpdHJlZQ==\n";
        let caps = parse_hello(line).unwrap();
        assert_eq!(caps.git.as_deref(), Some("/usr/bin/git"));
        assert_eq!(caps.delta, None);
        assert_eq!(caps.gh, None);
        assert_eq!(caps.runtime_dir, "/run/user/1000/alacritree");
    }

    #[test]
    fn rejects_unknown_hello_version() {
        assert!(parse_hello("hello\t2\t\t\t\t\n").is_none());
        assert!(parse_hello("goodbye\t1\t\t\t\t\n").is_none());
        assert!(parse_hello("hello\t1\t\t\n").is_none());
    }

    #[test]
    fn hello_with_empty_trailing_field_still_parses() {
        let caps = parse_hello("hello\t1\t\t\t\t\n").expect("empty fields are valid");
        assert_eq!(caps.git, None);
        assert_eq!(caps.runtime_dir, "");
    }

    #[test]
    fn reassembles_frames_across_split_reads() {
        let mut stream = Vec::new();
        stream.extend_from_slice(b"4\t0\t5\nhello");
        stream.extend_from_slice(b"9\t1\t0\n");
        let mut reader = FrameReader::default();
        let mut frames = Vec::new();
        // Byte-at-a-time is the worst case a pipe can deliver.
        for byte in stream {
            frames.extend(reader.push(&[byte]).unwrap());
        }
        assert_eq!(
            frames,
            vec![
                Frame { id: 4, exit: 0, payload: b"hello".to_vec() },
                Frame { id: 9, exit: 1, payload: Vec::new() },
            ]
        );
    }

    #[test]
    fn payload_bytes_are_binary_safe() {
        // NUL-delimited git porcelain, tabs, and newlines all pass through:
        // the header's byte count is the only framing.
        let payload = b"a\0b\tc\nd";
        let mut stream = format!("1\t0\t{}\n", payload.len()).into_bytes();
        stream.extend_from_slice(payload);
        let frames = FrameReader::default().push(&stream).unwrap();
        assert_eq!(frames, vec![Frame { id: 1, exit: 0, payload: payload.to_vec() }]);
    }

    #[test]
    fn malformed_header_is_a_protocol_error() {
        assert!(FrameReader::default().push(b"not a header\n").is_err());
        assert!(FrameReader::default().push(b"1\t0\n").is_err());
    }

    #[test]
    fn oversized_length_field_is_a_protocol_error_not_a_panic() {
        let header = format!("1\t0\t{}\n", usize::MAX);
        assert!(FrameReader::default().push(header.as_bytes()).is_err());
    }

    use std::path::Path;

    #[test]
    fn shim_invocation_builds_expected_argv() {
        let (program, args) = shim_invocation("kali-linux", Path::new(r"C:\proj"), "1234-1");
        assert_eq!(program, "wsl.exe");
        assert_eq!(
            args,
            vec![
                "-d",
                "kali-linux",
                "--cd",
                r"C:\proj",
                "--exec",
                "sh",
                "-c",
                SHIM_SCRIPT,
                "sh",
                "1234-1",
            ]
        );
    }

    #[test]
    fn wraps_bare_wsl_profile_for_default_distro() {
        let (args, distro) = wrap_profile_argv("wsl.exe", &[], "1234-2").unwrap();
        assert_eq!(distro, None);
        assert_eq!(args, vec!["--exec", "sh", "-c", SHIM_SCRIPT, "sh", "1234-2"]);
    }

    #[test]
    fn wraps_distro_and_cd_flags() {
        let profile_args: Vec<String> =
            ["-d", "kali-linux", "--cd", "/home"].iter().map(|s| s.to_string()).collect();
        let (args, distro) =
            wrap_profile_argv(r"C:\Windows\System32\wsl.exe", &profile_args, "9-9").unwrap();
        assert_eq!(distro.as_deref(), Some("kali-linux"));
        assert_eq!(
            args,
            vec![
                "-d",
                "kali-linux",
                "--cd",
                "/home",
                "--exec",
                "sh",
                "-c",
                SHIM_SCRIPT,
                "sh",
                "9-9"
            ]
        );
    }

    #[test]
    fn refuses_unparseable_profiles() {
        let to_vec = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // A positional command, an unknown flag, or a dangling value-flag may
        // not be a plain login shell — leave it alone (probes as unknown).
        assert!(wrap_profile_argv("wsl.exe", &to_vec(&["bash"]), "k").is_none());
        assert!(wrap_profile_argv("wsl.exe", &to_vec(&["-d", "kali", "htop"]), "k").is_none());
        assert!(wrap_profile_argv("wsl.exe", &to_vec(&["--exec", "sh"]), "k").is_none());
        assert!(wrap_profile_argv("wsl.exe", &to_vec(&["-d"]), "k").is_none());
        assert!(wrap_profile_argv("pwsh.exe", &[], "k").is_none());
        assert!(wrap_profile_argv("wslhost.exe", &[], "k").is_none());
    }

    #[test]
    fn probe_cache_lifecycle() {
        // An inert distro name: even if the poller ticks mid-test, `client()`
        // cools down on the failed spawn instead of touching a real distro.
        const D: &str = "no-such-distro";
        // Unknown key: unknown comm — the caller treats that as "no TUI".
        assert_eq!(foreground_comm(D, "test-77-1"), None);
        register_probe(D, "test-77-1");
        // Registered but not yet polled: still unknown, not a panic or a block.
        assert_eq!(foreground_comm(D, "test-77-1"), None);
        set_cached_comm(D, "test-77-1", Some("nvim".to_string()));
        assert_eq!(foreground_comm(D, "test-77-1").as_deref(), Some("nvim"));
        unregister_probe(D, "test-77-1");
        assert_eq!(foreground_comm(D, "test-77-1"), None);
    }

    #[test]
    fn probe_keys_are_pid_namespaced_and_unique() {
        let a = new_probe_key();
        let b = new_probe_key();
        assert_ne!(a, b);
        let prefix = format!("{}-", std::process::id());
        assert!(a.starts_with(&prefix), "{a} should start with {prefix}");
    }

    /// Live round trip against the default distro.  Requires WSL; run
    /// manually: `cargo test -p alacritree wsl_helper:: -- --ignored`
    #[test]
    #[ignore]
    fn helper_round_trips() {
        use std::time::{Duration, Instant};

        let distro =
            crate::wsl::distros().into_iter().find(|d| d.is_default).expect("a default distro");
        // Cold VM boot can take a while; the client comes up asynchronously.
        let deadline = Instant::now() + Duration::from_secs(120);
        let client = loop {
            if let Some(c) = client(&distro.name) {
                break c;
            }
            assert!(Instant::now() < deadline, "helper never became ready");
            std::thread::sleep(Duration::from_millis(200));
        };

        let caps = client.capabilities().expect("capabilities after ready");
        assert!(caps.git.is_some(), "test distros are expected to have git");
        assert!(caps.runtime_dir.ends_with("/alacritree"));

        let (exit, out) = client.run(r#"printf '%s' "$1""#, &["hello"]).expect("run");
        assert_eq!((exit, out.as_slice()), (0, &b"hello"[..]));

        // Empty args survive the `-` field encoding.
        let (_, out) = client.run(r#"printf '[%s][%s]' "$1" "$2""#, &["", "x"]).expect("run");
        assert_eq!(out, b"[][x]");

        // Payloads are binary-safe end to end.
        let (_, out) = client.run(r#"printf 'a\0b'"#, &[]).expect("run");
        assert_eq!(out, b"a\0b");

        // Concurrent jobs multiplex on one pipe without cross-talk.
        let slow = std::thread::spawn({
            let client = client.clone();
            move || client.run("sleep 1; printf slow", &[]).expect("slow run")
        });
        let (_, fast) = client.run("printf fast", &[]).expect("fast run");
        assert_eq!(fast, b"fast");
        assert_eq!(slow.join().unwrap().1, b"slow");

        // An unregistered probe key resolves to "no foreground comm".
        assert_eq!(client.probe("999999-999999").expect("probe"), None);

        use std::process::{Command, Stdio};

        use crate::command_ext::CommandExt;

        // The shim publishes its pid, then execs the login shell; piped stdin
        // (held open) keeps that shell alive for the duration of the test.
        // Without hide_console the spawn pops a visible terminal window when
        // the test runs from a hidden console.
        let key = new_probe_key();
        let (program, args) = shim_invocation(&distro.name, Path::new(r"C:\"), &key);
        let mut child = Command::new(program)
            .args(&args)
            .hide_console()
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn shimmed session");
        std::thread::sleep(Duration::from_secs(3));

        // The pidfile names a live, numeric pid...
        let (exit, out) = client
            .run(r#"cat "${XDG_RUNTIME_DIR:-/tmp}/alacritree/session-$1.pid" 2>/dev/null"#, &[&key])
            .expect("read pidfile");
        assert_eq!(exit, 0, "pidfile should exist for a shimmed session");
        let pid = String::from_utf8_lossy(&out);
        assert!(!pid.is_empty() && pid.chars().all(|c| c.is_ascii_digit()), "pid: {pid:?}");

        // ...and probing the idle shell resolves to "no foreground comm".
        // WSL2 allocates a controlling pty for every `--exec` session
        // regardless of the Windows-side stdio redirection, so the shell owns
        // the tty itself — the probe must read that as idle, not as a running
        // job, or every idle WSL session trips the close confirmation.
        let comm = client.probe(&key).expect("probe shimmed session");
        assert_eq!(comm, None, "idle shell should probe as no foreground job");

        let _ = child.kill();
        let _ = child.wait();
    }
}
