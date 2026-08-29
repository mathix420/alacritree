//! The boost off Windows: nothing, deliberately.
//!
//! See the module docs for why this is a decision rather than a gap.  A nice
//! value is inherited here, so a shell raised at spawn already covers what it
//! starts, and an unprivileged process cannot lower a nice value back anyway.
//! Whatever the answer is on these platforms, it is not this one.

/// Stands in for the Windows job object.  Never constructed, since [`adopt`]
/// has nothing to adopt into.
///
/// [`adopt`]: PriorityJob::adopt
pub struct PriorityJob;

impl PriorityJob {
    pub fn adopt(_pid: u32) -> Option<Self> {
        None
    }

    pub fn set_boosted(&self, _boosted: bool) {}
}

pub fn set_self_boosted(_boosted: bool) {}
