import type { APIRoute } from 'astro';
import { DMG, DMG_SHA256, DMG_SIGNATURE, VERSION } from '../../site';

// The app's update check reads this ("Check for Updates…", SWARM_UPDATE_FEED). It has the same
// fields as GitHub's "latest release" API, so the app parses either one, plus the DMG's sha256 and
// signature, without which the app offers only a download, not an install.
export const GET: APIRoute = () =>
  new Response(
    JSON.stringify(
      {
        tag_name: `v${VERSION}`,
        html_url: 'https://swarm-term.com/changelog/',
        draft: false,
        prerelease: false,
        assets: [
          {
            name: `Swarm-${VERSION}.dmg`,
            browser_download_url: DMG,
            ...(DMG_SIGNATURE && { sha256: DMG_SHA256, signature: DMG_SIGNATURE }),
          },
        ],
      },
      null,
      2,
    ),
    { headers: { 'Content-Type': 'application/json' } },
  );
