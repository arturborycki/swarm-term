export const VERSION = '0.1.0';
export const REPO = 'https://github.com/arturborycki/swarm-term';
/** While the repository is private, links into it are shown grayed out instead of leading to a 404. */
export const REPO_PUBLIC = false;
export const RELEASE = `${REPO}/releases/tag/v${VERSION}`;
/** Downloads come from the Cloudflare R2 bucket behind this domain (scripts/upload-release.sh), not GitHub. */
export const DOWNLOADS = 'https://downloads.swarm-term.com';
export const DMG = `${DOWNLOADS}/Swarm-${VERSION}.dmg`;
