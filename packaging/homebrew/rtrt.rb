# Homebrew formula for RTRT.
#
# Lives in this repo so the source of truth tracks the same git history as the
# binaries themselves. To publish a tap, copy this file to a separate
# `homebrew-tap` GitHub repo at the matching path
# (`Formula/rtrt.rb`) and update the `url`, `sha256`, and `version` lines for
# each release. The publish step is intentionally NOT automated from this repo
# — Homebrew taps are a user-controlled artefact.
#
# Local dry-run while developing the formula:
#
#   brew install --build-from-source --formula packaging/homebrew/rtrt.rb
#   brew test --build-from-source --formula packaging/homebrew/rtrt.rb
#
# When the first release ships, replace the URL + sha256 below with the
# released tarball, and bump the `version` line to match the tag.

class Rtrt < Formula
  desc "Retort — a Rust toolkit that distills AI agent context: compression, memory, multi-provider gateway, MCP server"
  homepage "https://github.com/kernalix7/rtrt"
  license "MIT"

  # Source-build path — fetches the matching tag's source tarball. After the
  # first release attach pre-built binaries to the GitHub Release and switch
  # this formula to the binary-install pattern (`url` → release asset,
  # `bin.install` the three executables). The placeholder URL/sha256 below
  # MUST be updated before publishing the tap; until then the formula is a
  # local-dev artefact only.
  url "https://github.com/kernalix7/rtrt/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  version "0.1.0"

  head "https://github.com/kernalix7/rtrt.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install",
           "--no-track",
           "--locked",
           "--path", "crates/rtrt-cli",
           "--root", prefix
    system "cargo", "install",
           "--no-track",
           "--locked",
           "--path", "crates/rtrt-mcp",
           "--root", prefix
    system "cargo", "install",
           "--no-track",
           "--locked",
           "--path", "crates/rtrt-dashboard",
           "--root", prefix
  end

  test do
    # Smoke: built binaries print their version + announce a known subcommand.
    assert_match version.to_s, shell_output("#{bin}/rtrt --version")
    assert_match "compress", shell_output("#{bin}/rtrt --help")
    assert_match "MCP", shell_output("#{bin}/rtrt-mcp --help")
  end
end
