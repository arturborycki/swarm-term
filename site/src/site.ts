export const VERSION = '0.1.6';
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
export const LINUX_SHA256_X86_64 = '333aa30209240c8635a1bc183eb9d42d40307f84fb75faf191da769a1708e75c';
export const LINUX_SHA256_AARCH64 = '6c133feb920a78a7c027221cc840c14094844f0b2a52884ee628fb4ecc9b3732';
/** The DMG's SHA-256 and the Ed25519 signature the app checks before installing it (scripts/sign-update.sh). */
export const DMG_SHA256 = '438fedd3c41a518cfc129cf56d68eb99d294f13dbedc133a696a380fc624955d';
export const DMG_SIGNATURE = 'cGqrRtbELaLFbDLfF/uChkeXgkh0MwNQfSjQTcaCxvLWlMRNDgsQll91j1VAFF7zOiV09kLJHnSYZQ056KhiBw==';
