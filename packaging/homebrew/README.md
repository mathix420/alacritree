# Homebrew packaging

Alacritree's Homebrew formula lives in **this repo** —
[`Formula/alacritree.rb`](../../Formula/alacritree.rb) — rather than a
dedicated `homebrew-alacritree` tap repo. That keeps everything in one
place at the cost of one extra step for users:

```sh
brew tap mathix420/alacritree https://github.com/mathix420/alacritree
brew install alacritree
```

(Homebrew's auto-tap only fires when the repo is named
`homebrew-<name>`. Since this repo isn't, users have to tap by URL
once. After that, `brew upgrade alacritree` Just Works.)

[`.github/workflows/homebrew-update.yml`](../../.github/workflows/homebrew-update.yml)
bumps the formula via an auto-merging PR on every published release —
exactly the same shape as `scoop-update.yml`. No Homebrew-specific
secret is required; the shared `ALACRITREE_BOT_TOKEN` covers it.

## Why a formula, not a cask?

Casks expect a `.app` bundle (or a `.dmg`/`.pkg` containing one).
Alacritree's macOS release tarball is just the bare binary today — the
desktop metadata and icons in the release workflow are gated on Linux.
If/when we start building a proper `Alacritree.app` (à la upstream
alacritty's `make app`), switching to a cask becomes worthwhile.
