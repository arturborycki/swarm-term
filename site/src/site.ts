export const VERSION = '0.1.8';
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
export const LINUX_SHA256_X86_64 = '5c835b129070b4c8ca1a5b4905951b33b33c636e0c77eb5fa8b4307abe4a29cf';
export const LINUX_SHA256_AARCH64 = 'f17e8415fd8b1d83d05d8eb194339c64ce87bb71868aef6b18bcb1fa635f2f80';
/** The DMG's SHA-256 and the Ed25519 signature the app checks before installing it (scripts/sign-update.sh). */
export const DMG_SHA256 = 'b285d7ca00ec8ea798a3c3b0c12599b5e0780ef63a0d4c7f7afd8bd393124396';
export const DMG_SIGNATURE = 'XQvCJxw4OHeaW/jpWT/bA53DELM86TlSXAttGPqmU/pO8q0zxeuwe27T5TUKxERC1mEQbgBKenE7/f2LcRlEAg==';
