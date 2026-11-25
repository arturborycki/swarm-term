export const VERSION = '0.1.11';
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
export const LINUX_SHA256_X86_64 = 'a8952743cd8a8850685efb08293f5ff13b15d0aeb298823bc94e7af76da7993e';
export const LINUX_SHA256_AARCH64 = '54c82794fa5291ac6c7334d1412bf9341d72ffcc61377cba83487e4935641fd0';
/** The DMG's SHA-256 and the Ed25519 signature the app checks before installing it (scripts/sign-update.sh). */
export const DMG_SHA256 = '699f74b1bcafcb095f73b1ddcdf455d41391ed91b9df1acf7f69563e785f6dd9';
export const DMG_SIGNATURE = 'dfs/kyrM82MvVoKghSSTeOw3HwulybWNxso1NUqKvmw0VHdLW3TyPwOBklAmbKwR8JxaQwsVS10MqBZf7JM1Dw==';
