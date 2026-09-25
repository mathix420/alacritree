//! Cost gate for the paths that run on every frame, in a test binary of its
//! own so the counting allocator is installed for nothing else.

use std::path::PathBuf;
use std::time::Instant;

use alacritree::alloc_count::{CountingAllocator, measure};
use alacritree::in_flight::InFlight;
use alacritree::multiplexer::Side;
use alacritree::projects::{Project, Worktree};
use alacritree::sidebar_focus::{ObservedInputs, SessionInput, UiInputs};
use alacritree::sidebar_model::{SidebarInputs, SidebarModel};
use alacritree_herdr::{self as herdr, FollowFocus};

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// `projects` × `worktrees` each.
fn tree(projects: usize, worktrees: usize) -> Vec<Project> {
    (0..projects)
        .map(|p| Project {
            root: PathBuf::from(format!("/home/user/code/p{p}")),
            name: format!("/home/user/code/p{p}"),
            label: None,
            default_branch: None,
            worktrees: (0..worktrees)
                .map(|w| {
                    let path = format!("/home/user/code/p{p}/worktree-{w}");
                    Worktree {
                        name: path.clone(),
                        path: PathBuf::from(path),
                        branch: None,
                        is_main: false,
                        prunable: false,
                        upstream: None,
                    }
                })
                .collect(),
            expanded: true,
            shell_override: None,
            home: None,
        })
        .collect()
}

/// Sessions carrying the titles a live query makes the compare walk. An
/// empty title compares without allocating whatever `matches` does with
/// it, so a fixture full of them cannot tell a borrowed comparison from
/// one that copies each title first.
fn sessions(count: usize) -> Vec<(Option<PathBuf>, u64, String)> {
    (0..count)
        .map(|i| {
            (
                Some(PathBuf::from(format!("/home/user/code/p0/worktree-{i}"))),
                i as u64,
                format!("nvim src/worktree-{i}.rs"),
            )
        })
        .collect()
}

fn inputs<'a>(
    s: &'a [(Option<PathBuf>, u64, String)],
) -> impl Iterator<Item = SessionInput<'a>> + Clone {
    s.iter().map(|(ws, id, title)| SessionInput { workspace: ws, id: *id, attention: false, title })
}

fn ui(query: &str, toggles: u32) -> UiInputs<'_> {
    UiInputs {
        session_rows_always: false,
        sessions_filter_counts_detached: false,
        query,
        toggles,
        toggles_apply: true,
        pr_generation: 0,
        active_workspace: None,
        active_branch: None,
        panes_generation: 0,
    }
}

/// A live query that every title matches exercises the per-title compare
/// with no toggle filtering narrowing anything on top of it.
#[test]
fn an_unchanged_frame_with_a_matching_query_allocates_nothing() {
    let projects = tree(10, 5);
    let live = sessions(150);
    let ui = ui("worktree", 0);
    let base = ObservedInputs::capture(&projects, inputs(&live), ui);

    let (same, counts) = measure(|| base.matches(&projects, inputs(&live), ui));

    assert!(same, "the fixture must actually be unchanged, or this measures the wrong path");
    assert_eq!(
        counts.allocs, 0,
        "an unchanged frame allocated {} times ({} bytes). The steady-state path has no \
         off-switch, so this is a per-frame tax on every user",
        counts.allocs, counts.bytes
    );
}

/// Toggle filters plus a query that only some titles match exercise the
/// narrower projection on top of the per-title compare.
#[test]
fn an_unchanged_frame_with_toggle_filters_and_a_narrow_query_allocates_nothing() {
    let projects = tree(10, 5);
    let live = sessions(150);
    let ui = ui("worktree-3", 0b11);
    let base = ObservedInputs::capture(&projects, inputs(&live), ui);

    let (same, counts) = measure(|| base.matches(&projects, inputs(&live), ui));

    assert!(same);
    assert_eq!(counts.allocs, 0, "a filter must not put an allocation back in the frame path");
}

/// The reconciler asks the row cache whether it is stale on every frame, so a
/// cache that answered by recapturing the inputs, or rebuilt the tree behind a
/// cache-shaped call, would put the cost back in the frame path.
#[test]
fn a_fresh_row_cache_and_a_reconciled_tree_allocate_nothing() {
    let projects = tree(10, 5);
    let live = sessions(150);
    let inputs =
        SidebarInputs { projects: &projects, sessions: inputs(&live), ui: ui("worktree", 0) };
    let mut model = SidebarModel::default();
    let stale = model.stale_rows(&inputs).expect("an empty model has no rows to reuse");
    model.fill_rows(stale, Vec::new(), Default::default());
    model.reconcile(&projects, &[], None);

    let ((stale, needs_reconcile), counts) =
        measure(|| (model.stale_rows(&inputs).is_some(), model.needs_reconcile()));

    assert!(!stale, "the fixture must actually be unchanged, or this measures the wrong path");
    assert!(!needs_reconcile);
    assert_eq!(
        counts.allocs, 0,
        "an unchanged frame allocated {} times ({} bytes) checking the row cache",
        counts.allocs, counts.bytes
    );
}

/// The poll runs every frame with no setting that disables it, so the
/// common case of nothing opening has to be free.
#[test]
fn polling_no_pending_spawns_allocates_nothing() {
    let mut spawns = InFlight::<u64, ()>::default();

    let (finished, counts) = measure(|| spawns.take_finished());

    assert!(finished.is_empty(), "nothing was started, so nothing can have finished");
    assert_eq!(
        counts.allocs, 0,
        "a frame with no PTY opening allocated {} times ({} bytes) polling for one",
        counts.allocs, counts.bytes
    );
}

/// Every mode but `always` discards whatever the trail records, and the
/// mode is read once at startup, so those users must pay nothing for it.
#[test]
fn a_mode_that_ignores_the_trail_allocates_nothing() {
    let panes = herdr::Listing::Panes.parse(
        r#"{"result":{"panes":[
            {"terminal_id":"t1","pane_id":"w1:p1","tab_id":"w1:t1","focused":true}
        ]}}"#,
    );
    let caches = vec![herdr::EndpointCache::for_test(Side::Native, panes, Instant::now())];

    for follow in [FollowFocus::Herdr, FollowFocus::Off] {
        let mut sync = herdr::HerdrViewSync::default();
        let (action, counts) = measure(|| {
            sync.next(herdr::ViewInputs {
                active: Some((1, None, false)),
                follow,
                caches: &caches,
                attentive: true,
                busy: false,
                now: Instant::now(),
                last_direct_input: None,
            })
        });

        assert!(action.is_none(), "mode {follow:?} acted on the trail");
        assert_eq!(
            counts.allocs, 0,
            "mode {follow:?} allocated {} times ({} bytes) recording a trail it discards",
            counts.allocs, counts.bytes
        );
    }
}

/// Not a gate. Run it by hand when changing the frame path:
/// `cargo test -p alacritree --release --test steady_state -- --ignored --nocapture`
#[test]
#[ignore = "timing harness, not an assertion"]
fn report_steady_state_cost() {
    for (p, w, s) in [(10, 5, 150), (50, 10, 500)] {
        let projects = tree(p, w);
        let live = sessions(s);
        let ui = ui("worktree", 0);
        let base = ObservedInputs::capture(&projects, inputs(&live), ui);

        // Warm the caches so the first iteration is not the whole sample.
        for _ in 0..1_000 {
            std::hint::black_box(base.matches(&projects, inputs(&live), ui));
        }

        let iterations = 100_000;
        let start = Instant::now();
        for _ in 0..iterations {
            std::hint::black_box(base.matches(&projects, inputs(&live), ui));
        }
        let each = start.elapsed() / iterations;

        println!("{p} projects x {w} worktrees, {s} sessions: {each:?} per unchanged frame");
    }
}
