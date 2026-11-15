class Buildwithnexus < Formula
  desc "Agentic AI coding CLI"
  homepage "https://buildwithnexus.dev"
  version "0.21.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/Garretts-Apps/buildwithnexus/releases/download/v0.21.0/buildwithnexus-aarch64-apple-darwin"
      sha256 "f406b6d59cf027967c43a68e5d2d69be958b6c0227040758e28ae4fe1dbe3fdc"
    end
    on_intel do
      url "https://github.com/Garretts-Apps/buildwithnexus/releases/download/v0.21.0/buildwithnexus-x86_64-apple-darwin"
      sha256 "1487d910e27321f36be5f57f7ab3fea2353750db584e3309f8a0aeb703bca426"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/Garretts-Apps/buildwithnexus/releases/download/v0.21.0/buildwithnexus-aarch64-unknown-linux-gnu"
      sha256 "807d5a04a2e81d0c92158f26e9a6f62b952238c0bc39279e4ee08d17e1144f68"
    end
    on_intel do
      url "https://github.com/Garretts-Apps/buildwithnexus/releases/download/v0.21.0/buildwithnexus-x86_64-unknown-linux-gnu"
      sha256 "3747acc3f91603c7e04b9439b227b2814df8098710ad27939e8b793d3a630a10"
    end
  end

  def install
    if OS.mac?
      src = Hardware::CPU.arm? ? "buildwithnexus-aarch64-apple-darwin" : "buildwithnexus-x86_64-apple-darwin"
    else
      src = Hardware::CPU.arm? ? "buildwithnexus-aarch64-unknown-linux-gnu" : "buildwithnexus-x86_64-unknown-linux-gnu"
    end
    bin.install src => "bwn"
    chmod 0755, bin/"bwn"
    bin.install_symlink "bwn" => "buildwithnexus"
  end

  test do
    assert_match "buildwithnexus #{version}", shell_output("#{bin}/bwn version")
  end
end
