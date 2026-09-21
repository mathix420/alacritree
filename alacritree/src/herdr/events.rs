//! herdr's event stream, read through `herdr remote-api-bridge`.
//!
//! The bridge relays stdio to the API socket herdr itself would use, so one
//! child process reaches a native server and a WSL one alike.  herdr serves
//! one request per connection and a subscription keeps its connection for
//! life, so every stream is its own bridge.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStderr, ChildStdout, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use super::PollError;
use super::cli::bridge_command;
use super::wire::{parse_pane_info, parse_status};
use crate::multiplexer::{Pane, PaneStatus, Side};
use crate::{command_ext, jobs};

/// Every kind that can change which panes a side has or how one renders,
/// apart from agent status, which herdr only streams per pane.
const LIFECYCLE: [&str; 9] = [
    "pane.created",
    "pane.closed",
    "pane.updated",
    "pane.focused",
    "pane.moved",
    "pane.exited",
    "pane.agent_detected",
    "tab.closed",
    "workspace.closed",
];

/// What one stream tells the endpoint that owns it.
#[derive(Debug)]
pub(super) enum Message {
    /// herdr accepted the subscription, and events follow.
    Started,
    Event(Event),
    /// The stream is over.  A stream that never started says why.
    Ended(Option<PollError>),
}

/// One event, reduced to what the endpoint does about it.
#[derive(Debug, Clone)]
pub(super) enum Event {
    Status {
        pane_id: String,
        status: PaneStatus,
    },
    Focused {
        pane_id: String,
    },
    Updated(Pane),
    /// Something the listing has to be read again to learn.
    Changed,
}

/// The subscription every side holds while its herdr is up.
pub(super) fn lifecycle_request() -> String {
    request(LIFECYCLE.iter().map(|kind| json!({ "type": kind })).collect())
}

/// Agent status for each of `pane_ids`.  herdr refuses the whole request when
/// one of them is unknown to it.
pub(super) fn status_request(pane_ids: &[String]) -> String {
    request(
        pane_ids
            .iter()
            .map(|pane_id| json!({ "type": "pane.agent_status_changed", "pane_id": pane_id }))
            .collect(),
    )
}

fn request(subscriptions: Vec<Value>) -> String {
    let request = json!({
        "id": "alacritree:events",
        "method": "events.subscribe",
        "params": { "subscriptions": subscriptions },
    });
    format!("{request}\n")
}

/// The first line a subscription answers with: the ack, or the code of the
/// error herdr refused it with.
fn decode_reply(line: &str) -> Result<(), String> {
    #[derive(Deserialize)]
    struct Reply {
        result: Option<Value>,
        error: Option<ErrorBody>,
    }
    #[derive(Deserialize)]
    struct ErrorBody {
        code: String,
    }
    let unexpected = || "unexpected_reply".to_string();
    let reply = serde_json::from_str::<Reply>(line).map_err(|_| unexpected())?;
    if let Some(error) = reply.error {
        return Err(error.code);
    }
    match reply.result.as_ref().and_then(|result| result.get("type")).and_then(Value::as_str) {
        Some("subscription_started") => Ok(()),
        _ => Err(unexpected()),
    }
}

/// One streamed event.  A line that is not an event is dropped rather than
/// guessed at.
fn decode_event(line: &str) -> Option<Event> {
    #[derive(Deserialize)]
    struct Envelope {
        event: String,
        #[serde(default)]
        data: Value,
    }
    let Envelope { event, data } = serde_json::from_str(line).ok()?;
    let field = |key: &str| data.get(key).and_then(Value::as_str).map(str::to_owned);
    Some(match event.as_str() {
        "pane.agent_status_changed" => Event::Status {
            pane_id: field("pane_id")?,
            status: parse_status(&field("agent_status")?),
        },
        "pane_focused" => Event::Focused { pane_id: field("pane_id")? },
        "pane_updated" => Event::Updated(parse_pane_info(data.get("pane")?.clone())?),
        _ => Event::Changed,
    })
}

/// Why a bridge that never started a stream exited.  The bridge prints its
/// connect failure in prose, and a herdr with no server running is the one
/// case worth waiting out.
fn classify_exit(stderr: &str) -> PollError {
    if stderr.contains("failed to connect") {
        PollError::Server("server_not_running".into())
    } else {
        PollError::Absent("bridge_unavailable")
    }
}

/// Wakes the UI once `delay` has passed, for a retry nothing else would
/// bring a frame for.
pub(super) fn wake_after(delay: Duration) {
    let _ = std::thread::Builder::new().name("alacritree-herdr-retry".into()).spawn(move || {
        std::thread::sleep(delay);
        jobs::pool().wake_ui();
    });
}

/// One subscription, from the spawn that starts its bridge to the bridge's
/// exit.  Dropping it kills the bridge, which is also how a subscription
/// ends, since herdr reads any byte or EOF on it as a close.
pub(super) enum Stream {
    /// The bridge is being spawned on the pool, since a `wsl.exe` start can
    /// take a loaded machine long enough to drop frames.
    Opening(jobs::Job<Result<Bridge, PollError>>),
    Open(Bridge),
    Closed,
}

impl Stream {
    pub(super) fn open(side: &Side, request: String) -> Self {
        let side = side.clone();
        Self::Opening(
            jobs::pool().spawn(jobs::Priority::Background, move |_| Bridge::spawn(&side, &request)),
        )
    }

    /// A stream that ends as soon as it is read, for a test build, which must
    /// never start a real bridge.
    pub(super) fn unreachable() -> Self {
        let (tx, rx) = mpsc::channel();
        let _ = tx.send(Message::Ended(Some(PollError::Absent("no_herdr_in_tests"))));
        Self::Open(Bridge { rx, child: None })
    }

    /// A stream the test drives through the returned sender.
    #[cfg(test)]
    pub(super) fn fake() -> (mpsc::Sender<Message>, Self) {
        let (tx, rx) = mpsc::channel();
        (tx, Self::Open(Bridge { rx, child: None }))
    }

    /// Everything the stream has said since the last call.  A bridge that
    /// could not start reads as a stream that ended, so its owner has one
    /// exit path to handle.  Never blocks.
    pub(super) fn messages(&mut self) -> Vec<Message> {
        if let Self::Opening(job) = self {
            let ended = |error| vec![Message::Ended(Some(error))];
            let (next, messages) = match job.poll() {
                Some(Ok(bridge)) => (Self::Open(bridge), Vec::new()),
                Some(Err(error)) => (Self::Closed, ended(error)),
                None if job.failed() => (Self::Closed, ended(PollError::Absent("spawn_panicked"))),
                None => return Vec::new(),
            };
            *self = next;
            if !messages.is_empty() {
                return messages;
            }
        }
        match self {
            Self::Open(bridge) => bridge.rx.try_iter().collect(),
            Self::Opening(_) | Self::Closed => Vec::new(),
        }
    }
}

/// A running `herdr remote-api-bridge` and the messages its reader sends.
pub(super) struct Bridge {
    rx: mpsc::Receiver<Message>,
    /// Holds the bridge's stdin open: herdr's Windows bridge stops relaying
    /// the moment its stdin reaches EOF.
    child: Option<Child>,
}

impl Bridge {
    /// Starts a bridge on `side` and sends `request` down it.  Runs on the
    /// pool, which is where [`Stream::open`] submits it.
    fn spawn(side: &Side, request: &str) -> Result<Self, PollError> {
        let spawn_failed = || PollError::Absent("spawn_failed");
        let (program, args) = bridge_command(side);
        #[allow(clippy::disallowed_methods)] // Always on a pool job.
        let mut child = command_ext::hidden(program)
            .args(args)
            .env("WSL_UTF8", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| spawn_failed())?;
        let sent = child.stdin.as_mut().is_some_and(|stdin| {
            stdin.write_all(request.as_bytes()).and_then(|()| stdin.flush()).is_ok()
        });
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(spawn_failed());
        };
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::Builder::new()
            .name("alacritree-herdr-events".into())
            .spawn(move || relay(stdout, stderr, &tx));
        // A bridge that died before reading its request still reports why on
        // stderr, which the reader collects at EOF.
        if reader.is_err() || (!sent && child.try_wait().ok().flatten().is_none()) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(spawn_failed());
        }
        Ok(Self { rx, child: Some(child) })
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Reads a bridge until it exits, waking the UI for every message.
fn relay(stdout: ChildStdout, mut stderr: ChildStderr, tx: &mpsc::Sender<Message>) {
    let send = |message| {
        let sent = tx.send(message).is_ok();
        jobs::pool().wake_ui();
        sent
    };
    let mut started = false;
    let mut refusal = None;
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        let message = if started {
            decode_event(&line).map(Message::Event)
        } else {
            match decode_reply(&line) {
                Ok(()) => {
                    started = true;
                    Some(Message::Started)
                },
                Err(code) => {
                    refusal = Some(code);
                    None
                },
            }
        };
        if let Some(message) = message
            && !send(message)
        {
            return;
        }
    }
    let reason = (!started).then(|| match refusal {
        Some(code) => PollError::Server(code),
        None => {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            classify_exit(&text)
        },
    });
    send(Message::Ended(reason));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ack_starts_a_stream() {
        assert_eq!(
            decode_reply(r#"{"id":"alacritree:events","result":{"type":"subscription_started"}}"#),
            Ok(())
        );
    }

    /// A status subscription naming a pane herdr no longer has is refused
    /// whole, with the code the endpoint logs.
    #[test]
    fn a_refusal_carries_herdr_code() {
        let line = r#"{"id":"alacritree:events","error":{"code":"pane_not_found","message":"no pane w1-9"}}"#;
        assert_eq!(decode_reply(line), Err("pane_not_found".into()));
        assert_eq!(decode_reply("not json"), Err("unexpected_reply".into()));
    }

    #[test]
    fn a_status_event_names_its_pane_and_state() {
        let line = r#"{"event":"pane.agent_status_changed","data":{"pane_id":"w1-2","workspace_id":"w1","agent_status":"working","agent":"claude","state_labels":{}}}"#;
        assert!(matches!(
            decode_event(line),
            Some(Event::Status { pane_id, status: PaneStatus::Working }) if pane_id == "w1-2"
        ));
    }

    #[test]
    fn a_focus_event_names_its_pane() {
        let line = r#"{"event":"pane_focused","data":{"type":"pane_focused","pane_id":"w1-3","workspace_id":"w1"}}"#;
        assert!(
            matches!(decode_event(line), Some(Event::Focused { pane_id }) if pane_id == "w1-3")
        );
    }

    /// `pane.updated` carries the whole pane in the shape `pane list` prints,
    /// so a title or cwd change patches the row without a listing.
    #[test]
    fn an_updated_pane_carries_its_row() {
        let line = r#"{"event":"pane_updated","data":{"type":"pane_updated","pane":{"pane_id":"w1-2","terminal_id":"term_a","workspace_id":"w1","tab_id":"w1:1","focused":false,"cwd":"/repo","agent":"claude","terminal_title_stripped":"review","agent_status":"idle","revision":4}}}"#;
        let Some(Event::Updated(pane)) = decode_event(line) else { panic!("not an update") };
        assert_eq!(pane.terminal_id, "term_a");
        assert_eq!(pane.title.as_deref(), Some("review"));
        assert_eq!(pane.status, Some(PaneStatus::Idle));
    }

    /// A kind alacritree has no patch for, including one a later herdr adds,
    /// sends it back to the listing.
    #[test]
    fn any_other_event_asks_for_a_listing() {
        let line = r#"{"event":"pane_closed","data":{"type":"pane_closed","pane_id":"w1-2","workspace_id":"w1"}}"#;
        assert!(matches!(decode_event(line), Some(Event::Changed)));
        let unheard_of = r#"{"event":"pane_teleported","data":{}}"#;
        assert!(matches!(decode_event(unheard_of), Some(Event::Changed)));
        assert!(decode_event("garbage").is_none());
    }

    #[test]
    fn a_status_request_names_every_pane() {
        let line = status_request(&["w1-1".into(), "w1-2".into()]);
        assert!(line.ends_with('\n'), "herdr reads one line per request");
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["method"], "events.subscribe");
        let subscriptions = request["params"]["subscriptions"].as_array().unwrap();
        assert_eq!(subscriptions.len(), 2);
        assert_eq!(subscriptions[1]["type"], "pane.agent_status_changed");
        assert_eq!(subscriptions[1]["pane_id"], "w1-2");
    }

    #[test]
    fn only_a_refused_connect_is_worth_waiting_out() {
        let refused = "error: failed to connect to remote Herdr API socket \
                       /home/u/.config/herdr/herdr.sock: No such file or directory";
        assert_eq!(classify_exit(refused), PollError::Server("server_not_running".into()));
        assert_eq!(
            classify_exit("sh: 1: exec: herdr: not found"),
            PollError::Absent("bridge_unavailable")
        );
    }
}
