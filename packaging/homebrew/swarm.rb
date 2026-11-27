# Homebrew cask for Swarm, for submitting to homebrew/homebrew-cask once the
# repository meets Homebrew's notability rules. scripts/update-homebrew.sh sets
# `version` and `sha256` after each release.
cask "swarm" do
  version "0.1.14"
  sha256 "3b45e3e46d1a07c0f78322fb00184c2898eb855e0949b597e69a61d8893617ff"

  url "https://github.com/arturborycki/swarm-term/releases/download/v#{version}/Swarm-#{version}.dmg"
  name "Swarm"
  desc "Terminal with persistent sessions, locally and over SSH"
  homepage "https://swarm-term.com/"

  livecheck do
    url :url
    strategy :github_latest
  end

  auto_updates true
  # The swarmd formula installs the same commands.
  conflicts_with formula: "swarmd"
  depends_on macos: :ventura

  app "Swarm.app"
  binary "#{appdir}/Swarm.app/Contents/MacOS/swarmctl"

  # Sessions live in swarmd: stopping it leaves them running until they end or
  # the Mac restarts.
  uninstall launchctl: "dev.swarm.swarmd",
            quit:      "dev.swarm.Swarm",
            script:    {
              executable:   "#{appdir}/Swarm.app/Contents/MacOS/swarmd",
              args:         ["stop"],
              must_succeed: false,
            }

  zap trash: [
    "~/Library/Application Support/dev.swarm.Swarm",
    "~/Library/Application Support/swarm",
    "~/Library/LaunchAgents/dev.swarm.swarmd.plist",
    "~/Library/Logs/swarm",
    "~/Library/Logs/swarm.log",
    "~/Library/Preferences/dev.swarm.Swarm.plist",
  ]
end
