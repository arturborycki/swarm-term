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
export const LINUX_SHA256_X86_64 = 'fec2fc30ca61d7738a40b1d21599f10ad95a24f3e3faa6cdba9ab71f4c0e75a3';
export const LINUX_SHA256_AARCH64 = '5634ce00b811bea585c9df4ed3122294db6b7ec35cbfad8632c767b7edc9d8ce';
/** The DMG's SHA-256 and the Ed25519 signature the app checks before installing it (scripts/sign-update.sh). */
export const DMG_SHA256 = '4851e1066e2c4ede2febc81c371c96fe758f3c2e3f27f5c2f9b45f77b338445a';
export const DMG_SIGNATURE = 'aq9aHvNwPHnsCK6N1DXvHT4N9Fa7ognA/AEhr+L2wpI2RiEHS3t1wbPlwLW53waRnBfuxtGCB5UuupDU2c7ADg==';
