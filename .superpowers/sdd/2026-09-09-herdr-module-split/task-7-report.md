# Task 7 report

## Result

Task 7 is implemented and committed in `e7e8e53724418f5e7977b0f787dfdf35143c83be`:

```text
refactor(herdr): move focus reconciliation out of app.rs
Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
```

No push or pull request was performed.

## Moved tests

The following eight tests moved from `app.rs` into `herdr/view.rs`:

- `herdr_shared_view_follows_new_tabs_and_refocuses_on_return`
- `herdr_shared_view_refocuses_after_an_ordinary_session`
- `herdr_follow_attempts_wait_for_a_new_snapshot`
- `herdr_shared_view_rejects_stale_and_foreign_focus_snapshots`
- `herdr_focus_completion_cannot_restore_a_view_left_while_pending`
- `a_shared_view_asks_herdr_for_its_pane`
- `a_direct_attach_never_asks_herdr_for_its_pane`
- `a_shared_view_asks_once_per_switch`

These tests remained in `app.rs` as required:

- `herdr_focus_completion_for_a_removed_session_is_ignored`, because it exercises `AlacritreeApp::sync_herdr_view_focus`.
- `an_agentless_pane_asks_herdr_for_its_pane_on_every_side`, because it was not in the move list.

## Integration changes

`herdr/view.rs` now owns:

- `HerdrViewFocus`
- `HerdrViewSync`
- `HerdrViewAction`
- `HerdrViewSync::{closed, attached, settled, next}`
- `needs_view_focus`
- the eight listed view tests

`herdr/cli.rs` now owns the unchanged attach gesture implementation:

- `HerdrAttachResult`
- `herdr_attach_gesture`

`herdr/mod.rs` declares `mod view` and re-exports the moved view and CLI interfaces. The moved public types, enum variants, functions, methods, and app-accessed fields received `pub` visibility. `view.rs` imports `AttachMode`, `jobs`, `SessionId`, and `super::{attaches_directly, Agent, HerdrKey, Side}`; its tests add `Duration` and `Listing`.

`app.rs` uses the `herdr::` prefix for every moved item:

- `herdr::HerdrViewSync` and `herdr::HerdrViewFocus` in app state and initialization.
- `herdr::herdr_attach_gesture` in the attach worker.
- `herdr::HerdrViewAction::{Focus, Follow}` in focus reconciliation.
- `herdr::HerdrViewFocus` in retained lifecycle tests.
- `herdr::HerdrAttachResult` in `PendingHerdrAttach`.
- `herdr::needs_view_focus` in the retained app test.

The `PendingHerdrAttach` state, app-state methods, accessor cluster, and presentation helpers stayed in `app.rs`.

## Verification

Commands and results:

- `cargo check -p alacritree` initially stopped before Rust compilation because the local zccache daemon was unavailable. Re-running with `RUSTC_WRAPPER=''` and `ZCCACHE_DISABLE=1` passed.
- Focused nextest run selected the eight `herdr::view::tests::*` tests plus the two retained app tests. Result: `10 tests run: 10 passed, 1558 skipped`.
- `git diff --cached --check` passed with no whitespace errors.
- The staged diff contained only the four requested source files: `325 insertions`, `300 deletions`.
- The signature scan against baseline `a039cf31` showed only the expected additions: `mod view`, the new `view` test module, `impl HerdrViewSync`, and the six moved public top-level declarations (`HerdrAttachResult`, `HerdrViewFocus`, `HerdrViewSync`, `HerdrViewAction`, `herdr_attach_gesture`, and `needs_view_focus`). No unrelated signature was added, removed, or duplicated.
- Full package-wide tests, full nextest, and clippy were not run per the narrowed verification request.

## Files changed

Code commit:

- `alacritree/src/herdr/view.rs` — created.
- `alacritree/src/herdr/cli.rs` — added the attach result and gesture.
- `alacritree/src/herdr/mod.rs` — added the module and re-exports.
- `alacritree/src/app.rs` — removed moved code/tests and prefixed integration call sites.

Report artifact:

- `.superpowers/sdd/2026-09-09-herdr-module-split/task-7-report.md` — written after the code commit; the review-fix evidence is included in the review-fix commit.

## Concerns

- `devrun task` could not be used because this checkout has no `devkit.toml`.
- The repository hook blocked stable `cargo fmt --check` and directed `devkit run task fmt`; that task is unavailable without `devkit.toml`. The code compiled and the staged whitespace check passed.
- Compilation emitted existing/re-export unused-import warnings in `herdr/mod.rs` for `can_attach`, `PaneInventory`, `PaneMetadata`, `Reach`, and the check-only `needs_view_focus` re-export. No new functional warning or failure appeared.
- The first manual `lockm acquire` returned `Access is denied`; the checkout write harness subsequently held the four task paths under the current session identity during the edit and commit. No lock was force-released.

## Review fix round 1

Restored the moved view test bodies' original qualifiers without changing production code:

- Added `use crate::herdr;` to the test module.
- Restored `herdr::Side`, `herdr::HerdrKey`, and `herdr::Listing` at all eight moved-test locations.
- Removed the test-only `use crate::herdr::Listing` import.
- Did not acquire, release, or remove the existing parent lock.

Verification:

- `cargo check -p alacritree` passed with `RUSTC_WRAPPER=''` and `ZCCACHE_DISABLE=1`; the existing `herdr/mod.rs` re-export warnings remain.
- Focused nextest run covered the eight `herdr::view::tests::*` tests and the two retained app tests: `10 tests run: 10 passed, 1558 skipped`.
- `git diff --check` passed.
- The source diff is limited to the test-module import and the eight qualifier restorations in `herdr/view.rs`.
- No full package suite or full nextest run was performed.
