export const VERSION = '0.1.9';
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
export const LINUX_SHA256_X86_64 = '71012865c6d53a4d8023acc097d942e3d54e0ca4941f8a34c990d98df0b6aeb1';
export const LINUX_SHA256_AARCH64 = 'a1405100a1a74daa1a9b87d0941170badf327d7298ccc6801e5ce1c4e765467b';
/** The DMG's SHA-256 and the Ed25519 signature the app checks before installing it (scripts/sign-update.sh). */
export const DMG_SHA256 = 'b154c6e0a46102533f318dfaf6a08227577709187bf21192100d52e6e1d82d04';
export const DMG_SIGNATURE = 'bKTtsGcq54rBwKFlYiICW8IUqHmOaWcG2nKGMzLDOAEanv1Xfw6sPvvzME+NGSfLSyShAsv5mMoZJqw0icxxCw==';
