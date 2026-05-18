class Alacritree < Formula
  desc "Alacritty fork with worktree-aware sidebars"
  homepage "https://github.com/mathix420/alacritree"
  version "0.2.6"
  license "Apache-2.0"

  # The release workflow only ships aarch64-apple-darwin today; Intel Macs
  # need to build from source until the release matrix grows an x86_64
  # target. The `on_macos` / `on_arm` guard makes the formula install fail
  # loudly on unsupported arches instead of silently downloading nothing.
  on_macos do
    on_arm do
      url "https://github.com/mathix420/alacritree/releases/download/v#{version}/alacritree-v#{version}-aarch64-macos.tar.gz"
      sha256 "73f7ce75f3b10d6f594c9c5471c0ec81ef2befc72f4557038b68e75e2182a90b"
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
