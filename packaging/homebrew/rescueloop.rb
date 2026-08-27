class Rescueloop < Formula
  desc "Lightweight local failure detection and AI-assisted repair console"
  homepage "https://github.com/ostapondo/rescueloop"
  version "0.1.0"
  license "Apache-2.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/ostapondo/rescueloop/releases/download/v#{version}/rescueloop-macos-aarch64.tar.gz"
      sha256 "RELEASE_WORKFLOW_REPLACES_THIS"
    else
      url "https://github.com/ostapondo/rescueloop/releases/download/v#{version}/rescueloop-macos-x86_64.tar.gz"
      sha256 "RELEASE_WORKFLOW_REPLACES_THIS"
    end
  end

  def install
    bin.install "rescueloop"
  end

  test do
    assert_match "RescueLoop", shell_output("#{bin}/rescueloop --help")
  end
end
