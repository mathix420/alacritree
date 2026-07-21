# Changelog

## [0.6.0](https://github.com/mathix420/alacritree/compare/v0.5.1...v0.6.0) (2026-07-21)


### Features

* add cross-workspace session cycling actions ([#124](https://github.com/mathix420/alacritree/issues/124)) ([403a1fe](https://github.com/mathix420/alacritree/commit/403a1fed48e8c82c1d580753e0d65cac30f042e2))
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
