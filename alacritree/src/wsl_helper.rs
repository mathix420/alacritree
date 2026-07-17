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
}
