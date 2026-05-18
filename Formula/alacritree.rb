class Alacritree < Formula
  desc "Alacritty fork with worktree-aware sidebars"
  homepage "https://github.com/mathix420/alacritree"
  version "0.2.7"
  license "Apache-2.0"

  # The release workflow only ships aarch64-apple-darwin today; Intel Macs
  # need to build from source until the release matrix grows an x86_64
  # target. The `on_macos` / `on_arm` guard makes the formula install fail
  # loudly on unsupported arches instead of silently downloading nothing.
  on_macos do
    on_arm do
      url "https://github.com/mathix420/alacritree/releases/download/v#{version}/alacritree-v#{version}-aarch64-macos.tar.gz"
      sha256 "854fe9c0d3cd9df12d8194a607a9a183fbd25b92a618783d039555620fd88b4e"
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
