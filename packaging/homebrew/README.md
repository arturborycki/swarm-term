# Homebrew

`swarm.rb` (cask: the app and `swarmctl`) and `swarmd.rb` (formula: the daemon and
`swarmctl`, built from source, for macOS and Linux) are written for Homebrew's own
repositories, so that `brew install --cask swarm` and `brew install swarmd` work without a
tap. They aren't submitted yet.

## What Homebrew requires

From Homebrew's [acceptance policy](https://docs.brew.sh/Package-Acceptance-Policy):

- The repository is at least 30 days old (`swarm-term`: from 2026-11-28).
- Notability, on `swarm-term`: 225 stars, 90 forks or 90 watchers when you submit it
  yourself, or 75 stars, 30 forks or 30 watchers when someone else does.
- The cask passes Gatekeeper (it's notarized); the formula builds from a tagged release.

## Checking the files

In a throwaway tap, with `T=$(brew --repository local/swarmcheck)` after
`brew tap-new --no-git local/swarmcheck`:

```bash
mkdir -p "$T/Casks" "$T/Formula" && cp swarm.rb "$T/Casks/" && cp swarmd.rb "$T/Formula/"
```

```bash
brew style local/swarmcheck/swarmd && brew style --cask local/swarmcheck/swarm
```

```bash
brew audit --new --formula local/swarmcheck/swarmd && brew audit --new --cask local/swarmcheck/swarm
```

```bash
brew install --build-from-source --formula local/swarmcheck/swarmd && brew test local/swarmcheck/swarmd
```

Until the notability numbers are met, the cask audit fails with "GitHub repository not
notable enough"; that should be its only complaint. Use `HOMEBREW_NO_AUTOREMOVE=1` when
uninstalling the test formula: otherwise `brew uninstall` also removes every formula
Homebrew considers an unused dependency.

## Submitting

Open a pull request adding `swarm.rb` to `Casks/s/` in
[homebrew-cask](https://github.com/Homebrew/homebrew-cask) and `swarmd.rb` to `Formula/s/`
in [homebrew-core](https://github.com/Homebrew/homebrew-core), without the comment at the
top of each file.
