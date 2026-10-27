// Shared by the background worker, popup and options page.

const DEFAULTS = {
  enabled: true,
  port: 47321,
  token: '',
  // Only downloads with these extensions are taken over from the browser.
  extensions: 'zip rar 7z gz bz2 xz tar tgz zst dmg pkg iso img exe msi deb rpm apk ' +
    'mp4 mkv avi mov webm m4v mp3 flac wav m4a aac pdf epub bin',
  // Ignore files smaller than this (when the size is known). 0 = no minimum.
  minSizeMB: 1,
  captureAll: false,
  // Show RDM's download dialog in the browser before sending.
  askInBrowser: true,
  // Show a Download button on YouTube's player.
  videoButton: true,
};

/** Sites RDM downloads through yt-dlp (kept in sync with the app). */
const VIDEO_SITES = ['youtube.com', 'youtu.be', 'vimeo.com', 'x.com', 'twitter.com', 'tiktok.com', 'instagram.com',
  'facebook.com', 'fb.watch', 'reddit.com', 'twitch.tv', 'dailymotion.com', 'soundcloud.com', 'bilibili.com',
  'streamable.com', 'loom.com', 'bandcamp.com', 'ted.com', 'nicovideo.jp'];

function isVideoSite(url) {
  try {
    const host = new URL(url).host.replace(/^(www|m)\./, '');
    return VIDEO_SITES.some((s) => host === s || host.endsWith(`.${s}`));
  } catch { return false; }
}

/** RDM's pairing tokens are 24 random bytes in hex. */
const TOKEN_LENGTH = 48;

async function getSettings() {
  return { ...DEFAULTS, ...(await chrome.storage.sync.get(null)) };
}

async function rdmFetch(path, options = {}) {
  const s = await getSettings();
  if (!s.token) throw new Error('Not paired yet: paste the token from RDM → Settings → Browser into the extension options.');
  let res;
  try {
    res = await fetch(`http://127.0.0.1:${s.port}${path}`, {
      ...options,
      headers: { 'Content-Type': 'application/json', 'X-RDM-Token': s.token, ...(options.headers || {}) },
    });
  } catch (e) {
    throw new Error(`Can't reach RDM on port ${s.port} (${e.message}). Is RDM running, and is the port the same as in RDM → Settings → Browser?`);
  }
  if (res.status === 401) {
    throw new Error(`RDM rejected the pairing token${s.token.length === TOKEN_LENGTH ? '' : ` (it should be ${TOKEN_LENGTH} characters, this one is ${s.token.length})`}. Click Copy in RDM → Settings → Browser and paste it again.`);
  }
  if (!res.ok) throw new Error(`RDM refused the request (${res.status}): ${await res.text()}`);
  return res.json();
}

/** Sends one download to RDM, with the browser's cookies for that URL. */
async function sendToRdm(url, extra = {}) {
  let cookies = '';
  try {
    cookies = (await chrome.cookies.getAll({ url })).map((c) => `${c.name}=${c.value}`).join('; ');
  } catch { /* no cookie access for this URL */ }
  return rdmFetch('/v1/downloads', {
    method: 'POST',
    body: JSON.stringify({ url, cookies, user_agent: navigator.userAgent, ...extra }),
  });
}

/** Resolves with RDM's info, or rejects if it isn't reachable within `ms`. */
function pingRdm(ms = 2000) {
  return Promise.race([
    rdmFetch('/v1/ping'),
    new Promise((_, reject) => setTimeout(() => reject(new Error('RDM did not answer in time.')), ms)),
  ]);
}
