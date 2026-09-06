//! Raise the session in front of the user above the load competing with it.
//!
//! A keystroke's round trip is mostly the program redrawing its own line, and
//! a line editor that highlights and completes as you type spends milliseconds
//! of CPU doing it.  At the same scheduling priority as a build saturating
//! every core, that work waits.  One class above the load restores the idle
//! figure; what it takes to get there is per-platform.
//!
//! Only Windows has an implementation, because only Windows has the problem in
//! this shape.  A priority class there does not spread to the processes a
//! raised process starts, so reaching them takes a job object.  A Unix nice
//! value *is* inherited, so a shell raised once already covers everything it
//! ever starts — and lowering a nice value back is privileged, which makes
//! "boost the focused session" the wrong shape for that platform rather than
//! merely unwritten.
//!
//! So the seam is a platform module behind one API, the way
//! `alacritty_terminal::tty` splits: callers hold a [`PriorityJob`] and ask
//! for a boost without knowing which platform they are on.  Elsewhere a job
//! is never created, every call is a no-op, and the `Option` costs a byte.

#[cfg(not(windows))]
mod other;
#[cfg(not(windows))]
pub use self::other::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::*;
