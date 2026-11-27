export const VERSION = '0.1.14';
export const REPO = 'https://github.com/arturborycki/swarm-term';
/** The app's source: the Warp fork (AGPL-3.0). */
export const APP_REPO = 'https://github.com/arturborycki/swarm-app';
/** While the repository is private, links into it are shown grayed out instead of leading to a 404. */
export const REPO_PUBLIC = true;
export const RELEASE = `${REPO}/releases/tag/v${VERSION}`;
/** The download link. worker/index.ts counts it, then redirects to the DMG on the GitHub release. */
export const DMG = `https://swarm-term.com/download/Swarm-${VERSION}.dmg`;
/** Linux builds of swarmd, swarm-shim and swarmctl, for installing on a server by hand (scripts/make-linux-tarballs.sh). */
export const linuxTarball = (arch: 'x86_64' | 'aarch64') =>
  `https://swarm-term.com/download/swarmd-${VERSION}-linux-${arch}.tar.gz`;
export const LINUX_SHA256_X86_64 = '7fdd7c912f49a34e2037db321408797bff5cba575fe605f85e9667a9ec5776ae';
export const LINUX_SHA256_AARCH64 = '284976c35c40c962d407e68e0a3cc86e0b8051f0961515268d1a3ed8c0fbd71b';
/** The DMG's SHA-256 and the Ed25519 signature the app checks before installing it (scripts/sign-update.sh). */
export const DMG_SHA256 = '3b45e3e46d1a07c0f78322fb00184c2898eb855e0949b597e69a61d8893617ff';
export const DMG_SIGNATURE = 'HPoG0Z7XsjaGF/7LDHnG/7hRbm2KBBhczidiI6w9mbZHgkqEZw/gZf5wOYQypRjnnVBwyuVKnlGCoyRU9OZeDg==';
