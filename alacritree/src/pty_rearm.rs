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
//! Wrapping the reader posts that packet.  Two things have to hold, and the
//! second is the one that bites.
//!
//! A visit must not post more packets than it consumes, or the queue doubles
//! every round until a wait returns nothing but stale packets — see `HANDBACK`.
//!
//! And a burst must end with a visit that posts nothing, or the queue never
//! returns to empty.  Posting one for one holds the depth steady rather than
//! draining it, so whatever depth a burst once reached it keeps, and the
//! writable packet carrying a Ctrl-C sits on the tail behind all of it while
//! the child keeps running.  The chain ends where `piper` takes the waker back,
//! which is the read that comes up empty — see `DRAIN_AHEAD`.
//!
//! A visit that cannot reach the terminal lock reads again without parsing.
//! Those reads consume no packet, so announcing on each of them posts a parse
//! buffer's worth of hand-backs for the one packet the visit took.  Painting
//! the grid is what holds that lock, so under load this is the ordinary case
//! rather than a rare one.  The loop offers its whole parse buffer at the top
//! of a visit and what is left of it on every read after — see `VisitBudget`.

use std::io::{self, Read};
use std::sync::Arc;

use alacritty_terminal::event::{OnResize, WindowSize};
use alacritty_terminal::event_loop::{MAX_LOCKED_READ, READ_BUFFER_SIZE};
use alacritty_terminal::tty::{
    ChildEvent, EventedPty, EventedReadWrite, PTY_READ_WRITE_TOKEN, Pty,
};
use polling::os::iocp::{CompletionPacket, PollerIocpExt};
use polling::{Event, PollMode, Poller};

/// How much one read may hand back.
///
/// The read loop reserves the terminal for as long as it runs and stops once
/// it has parsed `MAX_LOCKED_READ`, but it checks that cap only after parsing
/// whatever the last read returned.  A read carrying the whole cap therefore
/// ends the visit by itself, which is what holds an uncontended visit to one
/// announcement.  `take` fills the whole cap even across a refill for the same
/// reason: a short hand-back leaves the visit under the cap, so it reads once
/// more and announces once more.
///
/// Handing back less does not bound the parse — the cap already does that.  It
/// costs the visit a second read, and two packets per visit buy two visits,
/// which post four, and the backlog doubles until every wait returns a thousand
/// stale packets.  The loop drains the channel carrying keystrokes once per
/// wait, and the writable packet that finally carries a Ctrl-C to the child
/// goes on the tail of that queue, so it waits behind the whole backlog.
///
/// Taken from the read loop's own cap rather than copied, because falling
/// below it is what breaks `VisitBudget`: a hand-back short of the cap lets
/// the loop parse, reset `unprocessed`, and offer the whole buffer again, and
/// an offer that wide reads as a fresh visit with a fresh announcement.
const HANDBACK: usize = MAX_LOCKED_READ;

/// The ring between the console and the read loop, which `conpty` sizes from
/// the same constant.
const PIPE_CAPACITY: usize = READ_BUFFER_SIZE;

/// A backstop on one refill, not a target.
///
/// This has to sit above the ring.  A refill that stops before the ring runs
/// dry never asked `piper` for a byte it could not supply, so `piper` takes no
/// waker and only this side can announce what is left.  Every visit then
/// announces: one packet in, one packet out, and whatever depth the poller's
/// queue once reached it holds forever.  The writable packet carrying a Ctrl-C
/// is posted on the tail of that queue, so it waits behind all of it while the
/// child keeps running.
///
/// Reaching the empty read is what ends the chain.  The visit that empties the
/// staging goes quiet, the queue drains to nothing, and `piper` announces the
/// next byte.  Set below the ring, that never happens under load.
///
/// So the cap is only here to stop a filler thread that somehow outran a memcpy
/// from growing the staging without bound.
const DRAIN_AHEAD: usize = 2 * PIPE_CAPACITY;

const _: () = assert!(
    DRAIN_AHEAD > PIPE_CAPACITY,
    "a refill that cannot outrun the ring never reaches the read that hands the waker back, so \
     the announcements never stop",
);

/// Bytes taken out of the console pipe ahead of the read loop, and how far the
/// loop has got through them.
#[derive(Default)]
struct Staging {
    /// Kept at the high-water mark of what a refill has needed rather than
    /// resized per read, because a lock the renderer holds turns the read loop
    /// into a spin and every turn of it would otherwise zero a hand-back.
    /// Bytes past `filled` are whatever the last refill left.
    buf: Vec<u8>,
    filled: usize,
    taken: usize,
    /// Whether the last refill ended on a read that returned nothing, which is
    /// what `UnblockedReader` reports once `piper` has taken the waker.  A
    /// refill that stopped at `DRAIN_AHEAD` instead never asked for a byte
    /// `piper` could not supply, so no waker was installed and nothing but this
    /// side can announce what is left in the ring.
    ///
    /// The converse does not quite hold.  `piper` drops the waker on a
    /// non-empty ring and then, about once in a hundred drains, wakes it and
    /// reports nothing anyway so a fast reader cannot starve the writer.  That
    /// is indistinguishable from an empty ring here, and it has already queued
    /// a packet — so a refill it interrupts mid-staging leaves this side
    /// announcing on top of it, one extra packet in the queue until some visit
    /// finds the staging empty.
    rearmed: bool,
}

impl Staging {
    /// Empty `source` into the buffer, up to `DRAIN_AHEAD`.
    ///
    /// How far this got decides who announces next, so it records whether it
    /// reached the read that returned nothing.
    fn refill(&mut self, source: &mut impl Read) -> io::Result<()> {
        self.filled = 0;
        self.taken = 0;
        self.rearmed = false;

        while self.filled < DRAIN_AHEAD {
            let room = self.filled + HANDBACK;
            if self.buf.len() < room {
                self.buf.resize(room, 0);
            }
            match source.read(&mut self.buf[self.filled..room]) {
                Ok(0) => {
                    self.rearmed = true;
                    break;
                },
                Ok(read) => self.filled += read,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {},
                // `UnblockedReader` never reports this, and a source that did
                // would not have armed a waker on the way out, so leave the
                // announcement to this side.
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => return Err(err),
            }
        }

        // A burst leaves the high-water mark at `DRAIN_AHEAD`, which every
        // session that ever saw one would otherwise hold for its whole life.
        // A drained pipe with a hand-back or less behind it is the burst
        // ending, so give the room back and pay one growth if it resumes.
        if self.rearmed && self.filled <= HANDBACK && self.buf.len() > HANDBACK {
            self.buf.truncate(HANDBACK);
            self.buf.shrink_to_fit();
        }
        Ok(())
    }

    /// Hand the read loop a whole visit's worth, refilling as the buffer runs
    /// out, and stopping short only when the pipe has no more to give.
    ///
    /// Returns how much was handed back and whether anything is left for the
    /// caller to announce, either staged here or behind a ring nothing else
    /// will speak for.
    fn take(&mut self, out: &mut [u8], source: &mut impl Read) -> io::Result<(usize, bool)> {
        let want = out.len().min(HANDBACK);
        let mut read = 0;

        while read < want {
            if self.taken == self.filled {
                self.refill(source)?;
                if self.filled == 0 {
                    break;
                }
            }

            let end = (self.taken + want - read).min(self.filled);
            let staged = &self.buf[self.taken..end];
            out[read..read + staged.len()].copy_from_slice(staged);
            read += staged.len();
            self.taken = end;
        }

        Ok((read, self.taken < self.filled || !self.rearmed))
    }
}

/// Holds a visit to one announcement, however many times it reads.
///
/// A visit starts with the whole parse buffer and reads `buf[unprocessed..]`
/// after that, so an offer narrower than the widest yet seen is the loop going
/// round again on a lock it could not take.  Calibrating on the widest offer
/// rather than on a constant keeps this from mirroring a buffer size the read
/// loop owns.
///
/// The count is per visit rather than per opening read because the read that
/// has something to announce need not be the first.  An opening read that
/// finds the pipe nearly empty leaves the waker with `piper` and says nothing;
/// a burst landing before the next read fills the staging to `DRAIN_AHEAD`,
/// where `piper` holds no waker and this side is all that can speak.
#[derive(Default)]
struct VisitBudget {
    widest: usize,
    spent: bool,
}

impl VisitBudget {
    /// Whether a read offered this much room still has its visit's
    /// announcement to give.  A read that says nothing keeps it for the next.
    fn may_announce(&mut self, offered: usize) -> bool {
        if offered >= self.widest {
            self.widest = offered;
            self.spent = false;
        }
        !self.spent
    }

    fn spend(&mut self) {
        self.spent = true;
    }
}

/// Owns the PTY because `EventedReadWrite` hands out `&mut Self::Reader`, and
/// only the type holding the reader can supply that.
pub struct RearmingReader {
    pty: Pty,
    poller: Option<Arc<Poller>>,
    staged: Staging,
    visit: VisitBudget,
}

impl Read for RearmingReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Self { pty, staged, poller, visit } = self;
        let may_announce = visit.may_announce(buf.len());
        let (read, staged_behind) = staged.take(buf, pty.reader())?;

        if staged_behind
            && may_announce
            && let Some(poller) = poller
        {
            // A post that failed announced nothing, and after a refill that hit
            // the cap there is no waker either, so keep the budget for the next
            // read of this visit rather than leaving the staging unspoken for.
            let packet = CompletionPacket::new(Event::readable(PTY_READ_WRITE_TOKEN));
            if poller.post(packet).is_ok() {
                visit.spend();
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
        Self {
            reader: RearmingReader {
                pty,
                poller: None,
                staged: Staging::default(),
                visit: VisitBudget::default(),
            },
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// What the read loop hands a read: its parse buffer, always room for more
    /// than one hand-back.
    fn parse_buffer() -> Vec<u8> {
        vec![0; 4 * HANDBACK]
    }

    /// A source that gives back at most `chunk` bytes at a time, the way
    /// `piper` does when the reader is at the filling thread's heels.
    struct ShortReads {
        remaining: usize,
        chunk: usize,
    }

    impl Read for ShortReads {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let read = buf.len().min(self.chunk).min(self.remaining);
            buf[..read].fill(b'x');
            self.remaining -= read;
            Ok(read)
        }
    }

    /// A refill built out of short reads does not land on a multiple of the
    /// visit cap, so somewhere in it a hand-back runs out mid-visit.  Stopping
    /// there leaves the visit under the cap, so it reads again and announces
    /// again: one visit, two packets, and the backlog grows every refill.
    /// Crossing into the next refill instead keeps the visit to one read.
    #[test]
    fn a_short_read_still_fills_a_whole_handback() {
        let mut source = ShortReads { remaining: 64 * HANDBACK, chunk: 20_000 };
        let mut staging = Staging::default();
        let mut buf = parse_buffer();

        for take in 1..=8 {
            let (read, _) = staging.take(&mut buf, &mut source).unwrap();
            assert_eq!(read, HANDBACK, "take {take} left the visit short of its cap");
        }
    }

    /// A visit that cannot take the terminal lock reads again without parsing,
    /// and reads its way down `buf[unprocessed..]` until the buffer is full.
    /// Those reads consume no packet, so announcing on each of them hands the
    /// poller a buffer's worth for the one the visit took — sixteen for one at
    /// a megabyte buffer and a sixty-four kilobyte hand-back.
    #[test]
    fn a_visit_announces_once_however_often_it_reads() {
        let mut visit = VisitBudget::default();
        let buffer = 16 * HANDBACK;

        assert!(visit.may_announce(buffer), "a visit opens with its announcement to give");
        visit.spend();
        for unprocessed in (HANDBACK..buffer).step_by(HANDBACK) {
            let offered = buffer - unprocessed;
            assert!(
                !visit.may_announce(offered),
                "{offered} of {buffer} is the loop going round on a lock it could not take",
            );
        }
        assert!(visit.may_announce(buffer), "the next visit opens with one to give again");
    }

    /// The read with something to announce need not be the one that opened the
    /// visit.  An opening read that finds the pipe nearly empty leaves the
    /// waker with `piper` and says nothing; a burst landing before the next
    /// read fills the staging to `DRAIN_AHEAD`, where `piper` holds no waker
    /// and this side is all that can speak for what is staged.
    #[test]
    fn a_visit_that_opened_quiet_can_still_announce() {
        let mut visit = VisitBudget::default();
        let buffer = 16 * HANDBACK;

        assert!(visit.may_announce(buffer), "the opening read is offered its announcement");
        assert!(
            visit.may_announce(buffer - HANDBACK),
            "an opening read that said nothing leaves the announcement for the next",
        );
        visit.spend();
        assert!(!visit.may_announce(buffer - 2 * HANDBACK), "the visit has now spent it");
    }

    /// The read loop parses what a read returned before re-checking its own
    /// `MAX_LOCKED_READ` cap, so a take carrying the cap ends the visit by
    /// itself.  A shorter one costs the visit a second take, and every take
    /// that leaves bytes behind announces the pipe again — the packets one
    /// visit posts are the visits the next wait runs, so the backlog doubles.
    #[test]
    fn one_take_serves_a_whole_visit() {
        let mut source = io::Cursor::new(vec![b'x'; 4 * MAX_LOCKED_READ]);
        let mut staging = Staging::default();

        let (read, staged_behind) = staging.take(&mut parse_buffer(), &mut source).unwrap();

        assert!(read >= MAX_LOCKED_READ, "a visit stops after {MAX_LOCKED_READ}, got {read}");
        assert!(staged_behind, "the rest of the drain still has to be announced");
    }

    /// The whole ring arriving at once is the ordinary case under load, not an
    /// extreme.  A refill has to get through it and reach the read that comes
    /// up empty, because that is where `piper` takes the waker back and the
    /// chain of announcements ends.  A cap at or below the ring stops the
    /// refill first, every visit announces, and the poller's queue never
    /// returns to empty — which is what leaves a Ctrl-C waiting behind it.
    #[test]
    fn a_ring_sized_burst_hands_the_waker_back() {
        let mut source = io::Cursor::new(vec![b'x'; PIPE_CAPACITY]);
        let mut staging = Staging::default();

        staging.refill(&mut source).unwrap();

        assert!(staging.rearmed, "the refill stopped short of the pipe, so nobody holds the waker");
    }

    /// The staging grows to whatever the heaviest burst needed and the read
    /// loop never asks for it back, so a pane that once printed a large file
    /// would hold megabytes for the rest of its life — once per pane.
    #[test]
    fn a_burst_gives_its_room_back_when_the_pipe_drains() {
        let mut staging = Staging::default();

        staging.refill(&mut io::Cursor::new(vec![b'x'; PIPE_CAPACITY])).unwrap();
        assert!(staging.buf.capacity() >= PIPE_CAPACITY, "the burst should have grown the staging");

        staging.refill(&mut io::Cursor::new(Vec::new())).unwrap();

        let held = staging.buf.capacity();
        assert!(held <= 2 * HANDBACK, "a drained pipe left {held} bytes of staging behind");
    }

    /// Emptying the pipe leaves `piper` holding the waker, and that is what
    /// announces the next byte.  Announcing here as well queues a packet the
    /// loop wakes for and finds nothing behind.
    #[test]
    fn a_drained_pipe_announces_nothing() {
        let mut source = io::Cursor::new(b"hello".to_vec());
        let mut staging = Staging::default();

        assert_eq!(staging.take(&mut parse_buffer(), &mut source).unwrap(), (5, false));
        assert_eq!(staging.take(&mut parse_buffer(), &mut source).unwrap(), (0, false));
    }

    /// A refill that stopped at `DRAIN_AHEAD` never asked `piper` for a byte it
    /// could not supply, so `piper` holds no waker and the ring may still be
    /// full.  Handing back the last staged byte has to announce anyway: being
    /// wrong costs one wakeup that finds nothing, and staying quiet costs a
    /// pane that stops until the user types.
    #[test]
    fn a_refill_that_hit_the_cap_announces_its_last_byte() {
        let mut source = io::Cursor::new(vec![b'x'; 4 * DRAIN_AHEAD]);
        let mut staging = Staging::default();
        let mut buf = parse_buffer();

        let mut taken = 0;
        while taken < DRAIN_AHEAD {
            let (read, staged_behind) = staging.take(&mut buf, &mut source).unwrap();
            taken += read;
            assert!(staged_behind, "the ring was never drained, so only this side can announce");
        }
    }

    /// Every visit takes its cap and announces the remainder once, until the
    /// drain runs out.  Anything above one packet per visit compounds.
    #[test]
    fn a_drain_announces_once_per_visit() {
        let staged = 4 * MAX_LOCKED_READ + 100;
        let mut source = io::Cursor::new(vec![b'x'; staged]);
        let mut staging = Staging::default();
        let mut buf = parse_buffer();

        let mut announced = 0;
        let mut visits = 0;
        loop {
            let (read, staged_behind) = staging.take(&mut buf, &mut source).unwrap();
            if read == 0 {
                break;
            }
            visits += 1;
            announced += usize::from(staged_behind);
        }

        assert_eq!(visits, staged.div_ceil(MAX_LOCKED_READ));
        assert_eq!(announced, visits - 1, "only the last visit finds nothing staged behind it");
    }
}
