# Homebrew cask for Swarm. Publish it in a tap (e.g. arturborycki/homebrew-tap
# as Casks/swarm.rb) once DMGs are published as GitHub release assets:
#   brew install --cask arturborycki/tap/swarm
# Fill in `version` and `sha256` from scripts/make-dmg.sh for each release.
cask "swarm" do
  version "0.1.2"
  sha256 "d10158bd7068c809f20b4afaaa5052a61758c90cd877101a7259eeb69a43f898"

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
