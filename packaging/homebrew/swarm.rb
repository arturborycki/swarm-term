# Homebrew cask for Swarm. Publish it in a tap (e.g. arturborycki/homebrew-tap
# as Casks/swarm.rb) once DMGs are published as GitHub release assets:
#   brew install --cask arturborycki/tap/swarm
# Fill in `version` and `sha256` from scripts/make-dmg.sh for each release.
cask "swarm" do
  version "0.1.3"
  sha256 "e2274bd9a6420d4fc29151c80de3b2b92d26ea55724c1aa9aa03f1c2a3067cd3"

  url "https://github.com/arturborycki/swarm-term/releases/download/v#{version}/Swarm-#{version}.dmg"
  name "Swarm"
  desc "Terminal whose tabs are persistent sessions on this Mac or remote hosts"
  homepage "https://github.com/arturborycki/swarm-term"

  depends_on macos: ">= :ventura"

  app "Swarm.app"
  binary "#{appdir}/Swarm.app/Contents/MacOS/swarmctl"
  binary "#{appdir}/Swarm.app/Contents/MacOS/swarmd"

  # Sessions live in swarmd; stop it (they keep running until reboot) and
  # remove its login item on uninstall.
  uninstall quit:      "dev.swarm.Swarm",
            launchctl: "dev.swarm.swarmd",
            script:    {
              executable: "#{appdir}/Swarm.app/Contents/MacOS/swarmd",
              args:       ["stop"],
              must_succeed: false,
            }

  zap trash: [
    "~/Library/Application Support/dev.swarm.Swarm",
    "~/Library/Logs/swarm",
    "~/Library/Logs/swarm.log",
    "~/Library/Preferences/dev.swarm.Swarm.plist",
  ]
end
