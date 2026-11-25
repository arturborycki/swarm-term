export const VERSION = '0.1.12';
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
export const LINUX_SHA256_X86_64 = '2f383d4103e8fd2d4079a2f7b56bbdaf89f415de2e9e355b3b0333df57d08701';
export const LINUX_SHA256_AARCH64 = '6cb11ba2b83c670425065d38bd71e84e5f5393ef88e2ff2a2e2754744bd7e8ce';
/** The DMG's SHA-256 and the Ed25519 signature the app checks before installing it (scripts/sign-update.sh). */
export const DMG_SHA256 = '15b5950bae69f9b934dcbab4cf4a1722a2a5d666a3912d137bd645eee7520b37';
export const DMG_SIGNATURE = 'SK/srIbW03RXoIbePhjaTHk/zFkwfYOQ+HrCb305Q1JFQfcCT0we/hYCX9m6YVI3aDa2NA1532ISaLBkasiVDw==';
