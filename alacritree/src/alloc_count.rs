//! A counting allocator, since a timing threshold on a shared runner is
//! either flaky or too loose to show that a frame path allocates nothing.
//!
//! Nothing here installs it. `tests/steady_state.rs` does in a binary of its
//! own, and the library's unit-test build does for the timing reports.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

thread_local! {
    /// Counting is per-thread because the test harness itself allocates,
    /// tests run concurrently, and the app's own threads allocate whenever
    /// they like. Gating on the measuring thread is what makes the count
    /// attributable.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub allocs: usize,
    pub bytes: usize,
}

/// Run `f` with allocation counting on for this thread only.
///
/// Everything the assertion needs, such as formatting, panicking or `Vec`
/// growth in the caller, must happen outside the closure or it counts itself.
/// Counts stay at zero unless the binary installs `CountingAllocator` as its
/// `#[global_allocator]`.
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
