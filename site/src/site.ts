export const VERSION = '0.1.5';
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
export const LINUX_SHA256_X86_64 = '91410bfb45d02b66f81b5d8359c1519446b617ee607e1af41e701992491e7579';
export const LINUX_SHA256_AARCH64 = '56fcf2c56ff990686b68d452074963c26008dcb1ab2f5b20410106d0c529a6cb';
/** The DMG's SHA-256 and the Ed25519 signature the app checks before installing it (scripts/sign-update.sh). */
export const DMG_SHA256 = 'cb38b8f5cba4cd22c96499cba4f3f92bbc41a1957402aa5c2e43645a5d1cbb0b';
export const DMG_SIGNATURE = 'SDlo+OybGOKheQ67jgQLT++nNaxlTsyIF2rfd5y/GByQfpwV54LTtbUW6b/IuRH4ye+8OWAGK4E6eDj8NmF7BA==';
