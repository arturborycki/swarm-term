export const VERSION = '0.1.2';
export const REPO = 'https://github.com/arturborycki/swarm-term';
/** While the repository is private, links into it are shown grayed out instead of leading to a 404. */
export const REPO_PUBLIC = false;
export const RELEASE = `${REPO}/releases/tag/v${VERSION}`;
/**
 * The download link. worker/index.ts counts it, then redirects to the DMG in the Cloudflare R2
 * bucket behind downloads.swarm-term.com (scripts/upload-release.sh), not GitHub.
 */
export const DMG = `https://swarm-term.com/download/Swarm-${VERSION}.dmg`;
/** The DMG's SHA-256 and the Ed25519 signature the app checks before installing it (scripts/sign-update.sh). */
export const DMG_SHA256 = 'd10158bd7068c809f20b4afaaa5052a61758c90cd877101a7259eeb69a43f898';
export const DMG_SIGNATURE = 'r57AtH1HT9GpEa2arfMf+6Sa6Thu0KA2328CBQub4Z4l4TyBBEjR1ZxMnfGRJUqldV/VZcU7LZQBtKB5075OAA==';
