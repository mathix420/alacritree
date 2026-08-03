# Changelog

## [0.8.1](https://github.com/mathix420/alacritree/compare/v0.8.0...v0.8.1) (2026-08-03)


### Bug Fixes

* **mcp:** find socket without XDG_RUNTIME_DIR ([#169](https://github.com/mathix420/alacritree/issues/169)) ([1abb0f6](https://github.com/mathix420/alacritree/commit/1abb0f65cb5c6604cead163e2c8fd1c7e1063259))

## [0.8.0](https://github.com/mathix420/alacritree/compare/v0.7.1...v0.8.0) (2026-07-28)


### Features

* **palette:** add nav keys, sections, and columns ([#144](https://github.com/mathix420/alacritree/issues/144)) ([613393a](https://github.com/mathix420/alacritree/commit/613393a76194faf4c866987afd9938f2fc0e9d09))
* **sidebar:** add configurable path abbreviation styles [5] ([#152](https://github.com/mathix420/alacritree/issues/152)) ([497911b](https://github.com/mathix420/alacritree/commit/497911b7cce544234710412785cb265c9248db9d))
* **sidebar:** add search confirm/cancel actions [3] ([#138](https://github.com/mathix420/alacritree/issues/138)) ([d67c202](https://github.com/mathix420/alacritree/commit/d67c20255f32adb8f2b43e5b04a4540f20ab3d38))
* **sidebar:** keep the cursor when a row disappears [6] ([#153](https://github.com/mathix420/alacritree/issues/153)) ([ac6d3a5](https://github.com/mathix420/alacritree/commit/ac6d3a559c69a52feda9e7b9f21987b6f19976c2))
* **wsl:** advertise the binary path and hold the probe cache [1] ([#151](https://github.com/mathix420/alacritree/issues/151)) ([f637257](https://github.com/mathix420/alacritree/commit/f6372573426a45b524b572284261562ee74f8278))


### Bug Fixes

* **projects:** keep worktrees when unreachable ([#145](https://github.com/mathix420/alacritree/issues/145)) ([361ee88](https://github.com/mathix420/alacritree/commit/361ee8803108ca60a916e6225626e8b5f958b6f2))


### Performance Improvements

* **terminal:** cut input latency and output stalls [7] ([#154](https://github.com/mathix420/alacritree/issues/154)) ([2ea4408](https://github.com/mathix420/alacritree/commit/2ea44082fe0894ca9847117fef3673538cc071d1))

## [0.7.1](https://github.com/mathix420/alacritree/compare/v0.7.0...v0.7.1) (2026-07-23)


### Bug Fixes

* **macos:** use public process-group API ([#146](https://github.com/mathix420/alacritree/issues/146)) ([ddbd07e](https://github.com/mathix420/alacritree/commit/ddbd07e0d49c934fb1b343dfe009f3e5c04df017))

## [0.7.0](https://github.com/mathix420/alacritree/compare/v0.6.0...v0.7.0) (2026-07-23)


### Features

* **scratchpad:** add per-workspace notes ([#140](https://github.com/mathix420/alacritree/issues/140)) ([402b6c5](https://github.com/mathix420/alacritree/commit/402b6c5c8f2378d2fe7944e9b638aeacc8ac5ad6))


### Bug Fixes

* make missing worktree error dismissible ([#142](https://github.com/mathix420/alacritree/issues/142)) ([c1ad81c](https://github.com/mathix420/alacritree/commit/c1ad81c588969a5b4afd2cb02c1869d084bede76))
* **probe:** detect a nav-TUI anywhere in the foreground group ([#139](https://github.com/mathix420/alacritree/issues/139)) ([049b230](https://github.com/mathix420/alacritree/commit/049b23063935f901a1804c2659326ae0cd4b0dfa))

## [0.6.0](https://github.com/mathix420/alacritree/compare/v0.5.1...v0.6.0) (2026-07-21)


### Features

* add cross-workspace session cycling actions ([#124](https://github.com/mathix420/alacritree/issues/124)) ([403a1fe](https://github.com/mathix420/alacritree/commit/403a1fed48e8c82c1d580753e0d65cac30f042e2))
* add projects-sidebar keyboard actions ([#131](https://github.com/mathix420/alacritree/issues/131)) ([8e87b31](https://github.com/mathix420/alacritree/commit/8e87b319730af775ec6e2bd6c58b45aae29d7a47))
* **ipc:** share session id across the WSL boundary ([#132](https://github.com/mathix420/alacritree/issues/132)) ([0ab6d7e](https://github.com/mathix420/alacritree/commit/0ab6d7ecbb092fd38726032300d3604001341ebf))
* replace shortcuts window with a Ctrl+K command palette ([#133](https://github.com/mathix420/alacritree/issues/133)) ([e7ec0d2](https://github.com/mathix420/alacritree/commit/e7ec0d2e5d4b461783196b238c177c76840f52c9))
* **tabs:** hide session tab strip when only one session is open ([#130](https://github.com/mathix420/alacritree/issues/130)) ([32d0c07](https://github.com/mathix420/alacritree/commit/32d0c07deac27a7025ded032d88b14995732ea9b))


### Bug Fixes

* don't grab the parent console when stdout is already wired up ([#134](https://github.com/mathix420/alacritree/issues/134)) ([ec934d9](https://github.com/mathix420/alacritree/commit/ec934d93ea3a6f8c54b48beef3df6847f16d7421))

## [0.5.1](https://github.com/mathix420/alacritree/compare/v0.5.0...v0.5.1) (2026-07-20)


### Bug Fixes

* **macos:** dlopen libfontconfig to unblock the release build ([#125](https://github.com/mathix420/alacritree/issues/125)) ([1ff0fa3](https://github.com/mathix420/alacritree/commit/1ff0fa31b628d2e0a23a218422007b5444051c73))

## [0.5.0](https://github.com/mathix420/alacritree/compare/v0.4.1...v0.5.0) (2026-07-20)


### Features

* **config:** honor general.working_directory ([#113](https://github.com/mathix420/alacritree/issues/113)) ([93488be](https://github.com/mathix420/alacritree/commit/93488be3f6a10954b1231f6e3110f68ea60f4ec0))
* **ui:** debounce session attention pings ([#116](https://github.com/mathix420/alacritree/issues/116)) ([b7c2ffc](https://github.com/mathix420/alacritree/commit/b7c2ffcd6df638356c5c3842d5105cac5753dcbc))
* **ui:** focus sidebars on click ([#122](https://github.com/mathix420/alacritree/issues/122)) ([8412aa3](https://github.com/mathix420/alacritree/commit/8412aa3e3bf8f26683c5546da692f5375df73511))
* **wsl:** resident per-distro helper for probes and batched git [7] ([#110](https://github.com/mathix420/alacritree/issues/110)) ([652dc23](https://github.com/mathix420/alacritree/commit/652dc2340856014814c19e0bf7a4e4e01cb7e9d2))


### Bug Fixes

* **terminal:** drop pointer events under overlays ([#123](https://github.com/mathix420/alacritree/issues/123)) ([73d44d7](https://github.com/mathix420/alacritree/commit/73d44d77f930115caf85c8f22257b2516bf57438))
