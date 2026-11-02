export const VERSION = '0.1.1';
export const REPO = 'https://github.com/arturborycki/swarm-term';
/** While the repository is private, links into it are shown grayed out instead of leading to a 404. */
export const REPO_PUBLIC = false;
export const RELEASE = `${REPO}/releases/tag/v${VERSION}`;
/** Downloads come from the Cloudflare R2 bucket behind this domain (scripts/upload-release.sh), not GitHub. */
export const DOWNLOADS = 'https://downloads.swarm-term.com';
export const DMG = `${DOWNLOADS}/Swarm-${VERSION}.dmg`;
/** The DMG's SHA-256 and the Ed25519 signature the app checks before installing it (scripts/sign-update.sh). */
export const DMG_SHA256 = 'f580353a8d48b6529b0620ad2e93e0374e3792eff5ce6965d874f23604aeeec7';
export const DMG_SIGNATURE = 'KR3B3OM37+SK1Lk4+edQu4Xsi3tYmT6WgHObjhaDcSprTojf/g5GLor+Bkg8/JPw2cniiGYdUD7fLZHnM0O5Dw==';
