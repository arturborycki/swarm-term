export const VERSION = '0.1.4';
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
export const LINUX_SHA256_X86_64 = 'ef9512413d9abe207b9357d8b491bc139c7205e87a0d85a588f0d7ea0c7041ec';
export const LINUX_SHA256_AARCH64 = '6c9fce55e6f74cff192c1818885f54b835c99f24c148da606bef0749cf1f02c6';
/** The DMG's SHA-256 and the Ed25519 signature the app checks before installing it (scripts/sign-update.sh). */
export const DMG_SHA256 = '98b8c2f0ca470fa4e93b3c5561708931261069276416322488816e3e3865b9bd';
export const DMG_SIGNATURE = 'FLZaoQ5j5Vkr+MyeiB78wDYI4+EJkIL93URpiob9cS0kcWPQukpgJjzpzgg2Yz0AhdYiAJn1HHT0YXOlum3GAg==';
