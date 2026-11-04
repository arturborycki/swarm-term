# Security

## Reporting a problem

Please don't open a public issue for a security problem. Email
**info@swarm-term.com** with what you found and how to reproduce it, or use
GitHub's **Report a vulnerability** button on this repository's Security tab.
We'll reply, keep you posted while we work on a fix, and credit you in the
release notes if you'd like.

This covers `swarmd`, `swarm-shim`, `swarmctl` and the app
([swarm-app](https://github.com/arturborycki/swarm-app)), the release
downloads, and the update mechanism.

## Supported versions

Only the latest release gets fixes. Swarm installs updates itself, so most
people are on it within a day of a release.

## How releases are protected

- The macOS app is signed with a Developer ID and notarized by Apple.
- Every release DMG is signed with an Ed25519 release key that stays off this
  repository. The app installs an update only if the download matches the
  SHA-256 and signature in the release feed, and only if the new app is signed
  by the same Apple developer team.
- Sessions talk to `swarmd` over a socket only your user can open; remote hosts
  are reached through your own ssh setup.

More detail, including the dependency audit, is in
[docs/DESIGN.md §10](docs/DESIGN.md#10-security).
