export const VERSION = '0.1.3';
export const REPO = 'https://github.com/arturborycki/swarm-term';
/** While the repository is private, links into it are shown grayed out instead of leading to a 404. */
export const REPO_PUBLIC = false;
export const RELEASE = `${REPO}/releases/tag/v${VERSION}`;
/**
 * The download link. worker/index.ts counts it, then redirects to the DMG in the Cloudflare R2
 * bucket behind downloads.swarm-term.com (scripts/upload-release.sh), not GitHub.
 */
export const DMG = `https://swarm-term.com/download/Swarm-${VERSION}.dmg`;
/** Linux builds of swarmd, swarm-shim and swarmctl, for installing on a server by hand (scripts/make-linux-tarballs.sh). */
export const linuxTarball = (arch: 'x86_64' | 'aarch64') =>
  `https://swarm-term.com/download/swarmd-${VERSION}-linux-${arch}.tar.gz`;
export const LINUX_SHA256_X86_64 = 'd084a9a8896d8d4d0aa89aee6bdc89e27ef8a99a01d0af483919281160bdb3ad';
export const LINUX_SHA256_AARCH64 = '87e3c090d1f04d445a66d4285fd305f3830dc89b6e8048fbaf53dd46f6faaa5b';
/** The DMG's SHA-256 and the Ed25519 signature the app checks before installing it (scripts/sign-update.sh). */
export const DMG_SHA256 = 'e2274bd9a6420d4fc29151c80de3b2b92d26ea55724c1aa9aa03f1c2a3067cd3';
export const DMG_SIGNATURE = 'i4+Odux6CLH5Y+IREK5rX7kvhClFf4nGPhDRz4s5Lz+UfDMidtFgnPdGzQ74iH/WGyWGPsF7QJCz/9k/2wjqCw==';
