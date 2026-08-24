//! Where a frame's paint time goes, phase by phase.
//!
//! The frame harnesses time `ctx.run` end to end, which folds the grid walk,
//! the run vector and the record writes into one number alongside every bit of
//! egui's own work.  That says a frame got slower and cannot say which part
//! did, so a per-cell figure derived from it attributes cost to whatever the
//! reader already suspected.  These counters split the frame instead.
//!
//! Recording compiles only under `cfg(test)`: `phase!` expands to its body
//! alone otherwise, so a shipped frame runs the code it ran before.

/// Time one block against a phase.  Written `phase!(Capture, { .. })` with a
/// bare variant name so the enum is never named outside a test build.
macro_rules! phase {
    ($name:ident, $body:expr) => {{
        #[cfg(test)]
        let _timer = $crate::paint_phases::Timer::new($crate::paint_phases::Phase::$name);
        $body
    }};
}
pub(crate) use phase;

/// A glyph lookup that had to lay a galley out and write the atlas, rather
/// than reading a slot the table already held.  Only misses are counted: they
/// should fall to zero once a screen's characters are all seen, and a hit is
/// too cheap to be worth a counter in the middle of the write loop.
#[cfg(test)]
pub fn record_glyph_miss() {
    MISSES.with(|m| m.set(m.get() + 1));
}

#[cfg(not(test))]
#[inline(always)]
pub fn record_glyph_miss() {}

#[cfg(test)]
pub use recording::*;

#[cfg(test)]
mod recording {
    use std::cell::Cell;
    use std::time::{Duration, Instant};

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Phase {
        /// Walking the damaged rows and splitting them into styled runs.
        Capture,
        /// Clearing and rewriting the damaged rows' cell records.
        WriteRows,
        /// Re-emitting underlines and strikeouts as egui shapes.
        Decorations,
    }

    impl Phase {
        pub const ALL: [Phase; 3] = [Phase::Capture, Phase::WriteRows, Phase::Decorations];

        pub fn name(self) -> &'static str {
            match self {
                Phase::Capture => "capture",
                Phase::WriteRows => "write",
                Phase::Decorations => "decor",
            }
        }
    }

    thread_local! {
        /// Per-thread like `steady_state`'s counter and for the same reason: a
        /// process-wide total is unattributable once `cargo test` runs tests
        /// concurrently and the app's own threads paint whenever they like.
        static NANOS: [Cell<u64>; Phase::ALL.len()] = std::array::from_fn(|_| Cell::new(0));
        static CALLS: [Cell<u64>; Phase::ALL.len()] = std::array::from_fn(|_| Cell::new(0));
        pub(super) static MISSES: Cell<u64> = const { Cell::new(0) };
    }

    pub struct Timer {
        phase: Phase,
        started: Instant,
    }

    impl Timer {
        pub fn new(phase: Phase) -> Self {
            Self { phase, started: Instant::now() }
        }
    }

    impl Drop for Timer {
        fn drop(&mut self) {
            let nanos = self.started.elapsed().as_nanos() as u64;
            let i = self.phase as usize;
            NANOS.with(|n| n[i].set(n[i].get() + nanos));
            CALLS.with(|c| c[i].set(c[i].get() + 1));
        }
    }

    #[derive(Clone, Copy, Default, Debug)]
    pub struct PhaseTotal {
        pub elapsed: Duration,
        pub calls: u64,
    }

    #[derive(Clone, Copy, Default, Debug)]
    pub struct Totals {
        pub phases: [PhaseTotal; Phase::ALL.len()],
        pub glyph_misses: u64,
    }

    impl Totals {
        /// Per frame, for a run that accumulated `frames` of them.
        pub fn per_frame(self, frames: u32) -> Self {
            let mut out = self;
            for total in &mut out.phases {
                total.elapsed /= frames;
                total.calls /= u64::from(frames);
            }
            out.glyph_misses /= u64::from(frames);
            out
        }

        /// `capture 1.2ms/41 write 300µs/1 miss 0`, ready to print beside a
        /// frame's own timings.  Phases that never ran are left out.
        pub fn summary(self) -> String {
            let mut out = String::new();
            for (phase, total) in Phase::ALL.iter().zip(self.phases) {
                if total.calls == 0 {
                    continue;
                }
                out.push_str(&format!("{} {:?}/{} ", phase.name(), total.elapsed, total.calls));
            }
            out.push_str(&format!("miss {}", self.glyph_misses));
            out
        }
    }

    pub fn reset() {
        NANOS.with(|n| n.iter().for_each(|c| c.set(0)));
        CALLS.with(|c| c.iter().for_each(|c| c.set(0)));
        MISSES.with(|m| m.set(0));
    }

    pub fn totals() -> Totals {
        let mut phases = [PhaseTotal::default(); Phase::ALL.len()];
        NANOS.with(|n| {
            CALLS.with(|c| {
                for (i, total) in phases.iter_mut().enumerate() {
                    *total =
                        PhaseTotal { elapsed: Duration::from_nanos(n[i].get()), calls: c[i].get() };
                }
            })
        });
        Totals { phases, glyph_misses: MISSES.with(Cell::get) }
    }

    impl std::ops::AddAssign for Totals {
        fn add_assign(&mut self, rhs: Self) {
            for (into, from) in self.phases.iter_mut().zip(rhs.phases) {
                into.elapsed += from.elapsed;
                into.calls += from.calls;
            }
            self.glyph_misses += rhs.glyph_misses;
        }
    }
}
