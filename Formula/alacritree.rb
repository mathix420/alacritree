class Alacritree < Formula
  desc "Alacritty fork with worktree-aware sidebars"
  homepage "https://github.com/mathix420/alacritree"
  version "0.5.1"
  license "Apache-2.0"

  # Linked dynamically through the `fontconfig` Rust crate (alacritty's font
  # matching path); freetype is pulled in transitively but listed here too so
  # `brew uses --recursive` reflects the real link graph.
  depends_on "fontconfig"
  depends_on "freetype"
  # Runtime deps for the sidebar diff view: we shell out to `git diff … | delta`.
  # macOS preinstalls a system `git` via Command Line Tools, but pinning the
  # Homebrew one guarantees a recent enough version for the merge-base syntax.
  depends_on "git"
  depends_on "git-delta"

  # Releases also carry an x86_64-apple-darwin archive, but the update
  # workflow only stamps one sha256, so the formula stays arm-only for now.
  # The `on_macos` / `on_arm` guard makes the formula install fail loudly on
  # unsupported arches instead of silently downloading nothing.
  on_macos do
    on_arm do
      url "https://github.com/mathix420/alacritree/releases/download/v#{version}/alacritree-aarch64-apple-darwin.tar.gz"
      sha256 "94dfe4bb198b3a63009f112761119c16226dbe9d50490ddeb6b118734d5472ee"
    end
  end

  def install
    bin.install "alacritree"
  end

  test do
    # Don't invoke alacritree itself — it's a GUI that would spin up an
    # egui window during `brew test`. Just assert the binary landed.
    assert_predicate bin/"alacritree", :executable?
  end
end
