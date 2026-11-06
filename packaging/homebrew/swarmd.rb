# Homebrew formula for the Swarm daemon and swarmctl, built from source, for
# submitting to homebrew/homebrew-core once the repository meets Homebrew's
# notability rules. scripts/update-homebrew.sh sets the `url` and `sha256` after
# each release.
class Swarmd < Formula
  desc "Daemon and CLI for persistent terminal sessions, locally and over SSH"
  homepage "https://swarm-term.com/"
  url "https://github.com/arturborycki/swarm-term/archive/refs/tags/v0.1.4.tar.gz"
  sha256 "cf31cfcdc0398a8a08131cea2ef06ff099396b42a5137f240e6ee0c6fa38042d"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/arturborycki/swarm-term.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args(path: "crates/swarmd")
    system "cargo", "install", *std_cargo_args(path: "crates/swarmctl")
  end

  service do
    run [opt_bin/"swarmd", "run"]
    keep_alive true
  end

  test do
    ENV["SWARM_RUNTIME_DIR"] = testpath/"run"
    ENV["SWARM_STATE_DIR"] = testpath/"state"

    assert_match version.to_s, shell_output("#{bin}/swarmd --version")

    system bin/"swarmctl", "new", "--name", "brew-test", "--detached", "--", "/bin/sleep", "60"
    assert_match "brew-test", shell_output("#{bin}/swarmctl ls")
  ensure
    quiet_system bin/"swarmctl", "kill", "brew-test"
    quiet_system bin/"swarmctl", "rm", "brew-test"
    quiet_system bin/"swarmctl", "daemon", "stop"
  end
end
