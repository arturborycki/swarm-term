export const VERSION = '0.1.7';
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
export const LINUX_SHA256_X86_64 = '3c1ac00fc9c9b273fc244c0d7023c29d28a4e205e9c446bbf405b36383af146f';
export const LINUX_SHA256_AARCH64 = 'df15dd88e44829e539226d96b3a6c155773274212e89e78e5b5670455f99068d';
/** The DMG's SHA-256 and the Ed25519 signature the app checks before installing it (scripts/sign-update.sh). */
export const DMG_SHA256 = '5d353b5132c46ff746944ddca76f507d0217bc979b124735cb8c1c9063aeef39';
export const DMG_SIGNATURE = 'i+D78g1544jtXc4YpGDUbI9gfsok9RoMni2WsPfwbPmjL62Bl0h4B0E+jobDTfEJQVLjnOTBQIlW7jaRU3WwCg==';
