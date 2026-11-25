export const VERSION = '0.1.10';
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
export const LINUX_SHA256_X86_64 = '4274e4e8752e135efb9313011d9aa908cd35c9b6d95b19d4f88fc125516b6c62';
export const LINUX_SHA256_AARCH64 = 'fef9a98ed17c44e9bb34d7ace127bff8304c4b53b2acbea14873f56445fc0000';
/** The DMG's SHA-256 and the Ed25519 signature the app checks before installing it (scripts/sign-update.sh). */
export const DMG_SHA256 = '16d6b68955801d22d5ec98421d9e9e4c03357825ec32f7e0c0e2f4f2e3df7a24';
export const DMG_SIGNATURE = 'O0OQsrWjKe8cb7DdVukbMKsAVTvhaQ37F8jlG4FkYzt59FFc0bT4QhEHWyTKs4gVg61tK0UVIReBswFgBxKoDg==';
