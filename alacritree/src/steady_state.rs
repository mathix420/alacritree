//! Cost gate for the sidebar reconciler's per-frame path.
//!
//! The reconciler runs on every frame with no setting that disables it, so
//! "an unchanged frame allocates nothing" is a property the app depends on
//! rather than a target to aim at.  A counting allocator is the only way to
//! observe it: a timing threshold on a shared runner is either flaky or too
//! loose to detect anything.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

thread_local! {
    /// Counting is per-thread because a `#[global_allocator]` is process-wide
    /// and this crate has no library target to put an isolated test binary
    /// against: the test harness itself allocates, `cargo test` runs tests
    /// concurrently, and the app's own threads allocate whenever they like.
    /// Gating on the measuring thread is what makes the count attributable.
    static MEASURING: Cell<bool> = const { Cell::new(false) };
}

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);

pub struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if MEASURING.try_with(|m| m.get()).unwrap_or(false) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if MEASURING.try_with(|m| m.get()).unwrap_or(false) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(new_size, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub allocs: usize,
    pub bytes: usize,
}

/// Run `f` with allocation counting on for this thread only.  Everything the
/// assertion needs — formatting, panicking, `Vec` growth in the caller — must
/// happen outside the closure or it counts itself.
pub fn measure<T>(f: impl FnOnce() -> T) -> (T, Counts) {
    // Touch the TLS slot first: its own lazy initialisation allocates on some
    // platforms, and that allocation is not the one under test.
    MEASURING.with(|m| m.set(false));
    ALLOCS.store(0, Ordering::Relaxed);
    BYTES.store(0, Ordering::Relaxed);

    MEASURING.with(|m| m.set(true));
    let out = f();
    MEASURING.with(|m| m.set(false));

    (out, Counts { allocs: ALLOCS.load(Ordering::Relaxed), bytes: BYTES.load(Ordering::Relaxed) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar_focus::{self, ObservedInputs, SessionInput, UiInputs};
    use crate::sidebar_nav::tests::project;

    /// `projects` × `worktrees` each, with `sessions` sessions per worktree.
    fn tree(projects: usize, worktrees: usize) -> Vec<crate::projects::Project> {
        (0..projects)
            .map(|p| {
                let wts: Vec<String> =
                    (0..worktrees).map(|w| format!("/home/user/code/p{p}/worktree-{w}")).collect();
                let refs: Vec<&str> = wts.iter().map(String::as_str).collect();
                project(&format!("/home/user/code/p{p}"), true, &refs)
            })
            .collect()
    }

    fn sessions(count: usize) -> Vec<(Option<std::path::PathBuf>, u64)> {
        (0..count)
            .map(|i| {
                (
                    Some(std::path::PathBuf::from(format!("/home/user/code/p0/worktree-{i}"))),
                    i as u64,
                )
            })
            .collect()
    }

    fn inputs<'a>(
        s: &'a [(Option<std::path::PathBuf>, u64)],
    ) -> impl Iterator<Item = SessionInput<'a>> {
        s.iter().map(|(ws, id)| SessionInput { workspace: ws, id: *id, attention: false })
    }

    #[test]
    fn an_unchanged_frame_allocates_nothing() {
        let projects = tree(10, 5);
        let live = sessions(150);
        let ui = UiInputs {
            session_rows_always: false,
            query: "",
            toggles: 0,
            toggles_apply: true,
            pr_generation: 0,
            active_workspace: None,
            active_branch: None,
        };
        let base = ObservedInputs::capture(&projects, inputs(&live), ui);

        let (same, counts) = measure(|| base.matches(&projects, inputs(&live), ui));

        assert!(same, "the fixture must actually be unchanged, or this measures the wrong path");
        assert_eq!(
            counts.allocs, 0,
            "an unchanged frame allocated {} times ({} bytes) — the steady-state path has no \
             off-switch, so this is a per-frame tax on every user",
            counts.allocs, counts.bytes
        );
    }

    #[test]
    fn an_unchanged_filtering_frame_allocates_nothing() {
        let projects = tree(10, 5);
        let live = sessions(150);
        let ui = UiInputs {
            session_rows_always: false,
            query: "worktree-3",
            toggles: 0b11,
            toggles_apply: true,
            pr_generation: 0,
            active_workspace: None,
            active_branch: None,
        };
        let base = ObservedInputs::capture(&projects, inputs(&live), ui);

        let (same, counts) = measure(|| base.matches(&projects, inputs(&live), ui));

        assert!(same);
        assert_eq!(counts.allocs, 0, "a filter must not put an allocation back in the frame path");
    }

    #[test]
    fn the_compare_is_linear_in_the_tree_size() {
        let small = tree(10, 5);
        let big = tree(50, 10);
        let ui = UiInputs {
            session_rows_always: false,
            query: "",
            toggles: 0,
            toggles_apply: true,
            pr_generation: 0,
            active_workspace: None,
            active_branch: None,
        };

        let base_small = ObservedInputs::capture(&small, std::iter::empty(), ui);
        sidebar_focus::reset_visits();
        assert!(base_small.matches(&small, std::iter::empty(), ui));
        let small_visits = sidebar_focus::visits();

        let base_big = ObservedInputs::capture(&big, std::iter::empty(), ui);
        sidebar_focus::reset_visits();
        assert!(base_big.matches(&big, std::iter::empty(), ui));
        let big_visits = sidebar_focus::visits();

        // 50×10 is 10× the records of 10×5.  Linear work lands near 10×;
        // anything quadratic lands near 100× and trips this well before a
        // timing threshold would notice.
        assert!(
            big_visits < small_visits * 20,
            "comparing a 10× larger tree examined {big_visits} records against {small_visits} \
             — that is superlinear, so something is scanning inside a per-node loop"
        );
    }

    /// Not a gate — run it by hand when changing the frame path:
    /// `cargo test -p alacritree --release -- --ignored --nocapture steady_state`
    #[test]
    #[ignore = "timing harness, not an assertion"]
    fn report_steady_state_cost() {
        for (p, w, s) in [(10, 5, 150), (50, 10, 500)] {
            let projects = tree(p, w);
            let live = sessions(s);
            let ui = UiInputs {
                session_rows_always: false,
                query: "",
                toggles: 0,
                toggles_apply: true,
                pr_generation: 0,
                active_workspace: None,
                active_branch: None,
            };
            let base = ObservedInputs::capture(&projects, inputs(&live), ui);

            // Warm the caches so the first iteration is not the whole sample.
            for _ in 0..1_000 {
                std::hint::black_box(base.matches(&projects, inputs(&live), ui));
            }

            let iterations = 100_000;
            let start = std::time::Instant::now();
            for _ in 0..iterations {
                std::hint::black_box(base.matches(&projects, inputs(&live), ui));
            }
            let each = start.elapsed() / iterations;

            println!("{p} projects x {w} worktrees, {s} sessions: {each:?} per unchanged frame");
        }
    }
}
