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
            let frame_end = newline + 1 + len;
            if self.buf.len() < frame_end {
                return Ok(frames);
            }
            frames.push(Frame { id, exit, payload: self.buf[newline + 1..frame_end].to_vec() });
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
      tpgid=${6:-}
      case $tpgid in ''|*[!0-9]*) tpgid= ;; esac
      [ -n "$tpgid" ] && comm=$(cat "/proc/$tpgid/comm" 2>/dev/null)
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
    let stem = Path::new(program).file_stem()?.to_str()?;
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
}
