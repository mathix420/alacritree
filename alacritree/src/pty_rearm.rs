//! Supplies the level-triggered console readiness that the PTY reader accepts
//! but never sends.
//!
//! Windows has no readiness to report for a console pipe, so the reader
//! emulates it: a completion packet reaches the poller through the waker
//! `piper` holds for its pipe.  `piper` installs that waker only when a drain
//! comes up empty, which is the ordinary contract — poll until pending.  The
//! reader takes `PollMode::Level` and then never posts a packet of its own
//! after a drain returns data, so a drain that leaves bytes behind ends with
//! the pipe holding data and nothing able to announce it.  The loop sleeps
//! until something unrelated arrives — a keystroke, a resize, the child
//! exiting — which is why a pane streaming megabytes only advances while the
//! user types.
//!
//! Wrapping the reader posts that packet.  Every read carrying data announces
//! the pipe again, so the loop keeps coming back until a drain finally comes
//! up empty and `piper` takes over.  Announcing after the read that empties
//! the pipe costs one wakeup that finds nothing; skipping it would restore the
//! stall, because then no drain ever reaches the pending path that installs
//! the waker.

use std::io::{self, Read};
use std::sync::Arc;

use alacritty_terminal::event::{OnResize, WindowSize};
use alacritty_terminal::tty::{
    ChildEvent, EventedPty, EventedReadWrite, PTY_READ_WRITE_TOKEN, Pty,
};
use polling::os::iocp::{CompletionPacket, PollerIocpExt};
use polling::{Event, PollMode, Poller};

/// How much one read may hand back.
///
/// The read loop reserves the terminal for as long as it runs and stops once
/// it has parsed its own cap, but it checks that cap only after parsing
/// whatever the last read returned — so a single drain the size of its buffer
/// becomes a single parse of the same size, and the pane holds the grid for as
/// long as that takes.  A console delivers hundreds of kilobytes at a time
/// under load, which is tens of milliseconds the UI thread spends queued for a
/// lock it needs every frame.  Handing back a slice instead splits that into
/// batches the loop can be interrupted between.
const READ_CHUNK: usize = 32 * 1024;

/// Owns the PTY because `EventedReadWrite` hands out `&mut Self::Reader`, and
/// only the type holding the reader can supply that.
pub struct RearmingReader {
    pty: Pty,
    poller: Option<Arc<Poller>>,
}

impl Read for RearmingReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let chunk = buf.len().min(READ_CHUNK);
        let read = self.pty.reader().read(&mut buf[..chunk])?;
        if read > 0 {
            if let Some(poller) = &self.poller {
                let _ = poller.post(CompletionPacket::new(Event::readable(PTY_READ_WRITE_TOKEN)));
            }
        }
        Ok(read)
    }
}

pub struct RearmingPty {
    reader: RearmingReader,
}

impl RearmingPty {
    pub fn new(pty: Pty) -> Self {
        Self { reader: RearmingReader { pty, poller: None } }
    }
}

impl EventedReadWrite for RearmingPty {
    type Reader = RearmingReader;
    type Writer = <Pty as EventedReadWrite>::Writer;

    unsafe fn register(
        &mut self,
        poll: &Arc<Poller>,
        interest: Event,
        poll_opts: PollMode,
    ) -> io::Result<()> {
        self.reader.poller = Some(poll.clone());
        unsafe { self.reader.pty.register(poll, interest, poll_opts) }
    }

    fn reregister(
        &mut self,
        poll: &Arc<Poller>,
        interest: Event,
        poll_opts: PollMode,
    ) -> io::Result<()> {
        self.reader.poller = Some(poll.clone());
        self.reader.pty.reregister(poll, interest, poll_opts)
    }

    fn deregister(&mut self, poll: &Arc<Poller>) -> io::Result<()> {
        self.reader.poller = None;
        self.reader.pty.deregister(poll)
    }

    fn reader(&mut self) -> &mut Self::Reader {
        &mut self.reader
    }

    fn writer(&mut self) -> &mut Self::Writer {
        self.reader.pty.writer()
    }
}

impl EventedPty for RearmingPty {
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        self.reader.pty.next_child_event()
    }
}

impl OnResize for RearmingPty {
    fn on_resize(&mut self, window_size: WindowSize) {
        self.reader.pty.on_resize(window_size);
    }
}
