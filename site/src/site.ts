export const VERSION = '0.1.15';
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
export const LINUX_SHA256_X86_64 = '004e10d96a3f4332c4210b847f8785dd344ccf01997df4dc45b88183ba42791e';
export const LINUX_SHA256_AARCH64 = 'ed69b0a40b5fc8c2c101ccd13f8413774307d8076c1170dc5f89c7c3220182d2';
/** The DMG's SHA-256 and the Ed25519 signature the app checks before installing it (scripts/sign-update.sh). */
export const DMG_SHA256 = 'f2af88046919d19e9d7beea6db4b0d343b47c293b356413f46018e53af9d8294';
export const DMG_SIGNATURE = 'tTFVsivUBFkxgVKxXxR7VmXx9ygxBiE0dGoLDOzdDV0+gxmrnwLoPv8wB1S+fCmW0pd4g1zJFbE8ro5zhgULAQ==';
