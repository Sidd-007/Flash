// Shared by the background worker, popup and options page.

const DEFAULTS = {
  enabled: true,
  port: 47321,
  // Only downloads with these extensions are taken over from the browser.
  extensions: 'zip rar 7z gz bz2 xz tar tgz zst dmg pkg iso img exe msi deb rpm apk ' +
    'mp4 mkv avi mov webm m4v mp3 flac wav m4a aac pdf epub bin',
  // Ignore files smaller than this (when the size is known). 0 = no minimum.
  minSizeMB: 1,
  captureAll: false,
  // Show Flash's download dialog in the browser before sending.
  askInBrowser: true,
  // Show a Download button on YouTube's player.
  videoButton: true,
  // Sites (hosts) where it doesn't show, separated by spaces or commas.
  videoButtonExclude: '',
};

/** Sites Flash downloads through yt-dlp (kept in sync with the app). */
const VIDEO_SITES = ['youtube.com', 'youtu.be', 'vimeo.com', 'x.com', 'twitter.com', 'tiktok.com', 'instagram.com',
  'facebook.com', 'fb.watch', 'reddit.com', 'twitch.tv', 'dailymotion.com', 'soundcloud.com', 'bilibili.com',
  'streamable.com', 'loom.com', 'bandcamp.com', 'ted.com', 'nicovideo.jp'];

function isVideoSite(url) {
  try {
    const host = new URL(url).host.replace(/^(www|m)\./, '');
    return VIDEO_SITES.some((s) => host === s || host.endsWith(`.${s}`));
  } catch { return false; }
}

/** Flash's pairing tokens are 24 random bytes in hex. */
const TOKEN_LENGTH = 48;

/** Preferences, which sync between the user's browsers. Never the token. */
async function getSettings() {
  const { token: _, ...prefs } = await chrome.storage.sync.get(null);
  return { ...DEFAULTS, ...prefs };
}

/**
 * The pairing token. It belongs to this computer's Flash, so it lives in
 * local storage, which never syncs. Only extension pages and the background
 * worker read it; the YouTube content script asks the worker instead.
 */
async function getToken() {
  const [{ token }, { token: synced }] = await Promise.all([
    chrome.storage.local.get('token'),
    chrome.storage.sync.get('token'),
  ]);
  // Older versions kept it in sync storage, which copies it to other browsers.
  // A local token is the newer one; either way the synced copy goes.
  if (synced !== undefined) {
    if (!token && synced) await chrome.storage.local.set({ token: synced });
    await chrome.storage.sync.remove('token');
  }
  return token || synced || '';
}

/**
 * Keeps content scripts out of local storage, where the token lives, so only
 * extension pages and the background worker can read it. Older Chrome and
 * Firefox may not support this for local storage; then the token is still
 * only ever read by trusted pages.
 */
async function restrictLocalStorage() {
  try {
    await chrome.storage.local.setAccessLevel({ accessLevel: 'TRUSTED_CONTEXTS' });
    return true;
  } catch {
    return false;
  }
}

async function setToken(token) {
  await chrome.storage.local.set({ token });
  await chrome.storage.sync.remove('token');
}

async function rdmFetch(path, options = {}) {
  const s = await getSettings();
  const token = await getToken();
  if (!token) throw new Error('Not paired yet: paste the token from Flash → Settings → Browser into the extension options.');
  let res;
  try {
    res = await fetch(`http://127.0.0.1:${s.port}${path}`, {
      ...options,
      headers: { 'Content-Type': 'application/json', 'X-RDM-Token': token, ...(options.headers || {}) },
    });
  } catch (e) {
    throw new Error(`Can't reach Flash on port ${s.port} (${e.message}). Is Flash running, and is the port the same as in Flash → Settings → Browser?`);
  }
  if (res.status === 401) {
    throw new Error(`Flash rejected the pairing token${token.length === TOKEN_LENGTH ? '' : ` (it should be ${TOKEN_LENGTH} characters, this one is ${token.length})`}. Click Copy in Flash → Settings → Browser and paste it again.`);
  }
  if (!res.ok) throw new Error(`Flash refused the request (${res.status}): ${await res.text()}`);
  return res.json();
}

/** Sends one download to Flash, with the browser's cookies for that URL. */
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

/** Resolves with Flash's info, or rejects if it isn't reachable within `ms`. */
function pingRdm(ms = 2000) {
  return Promise.race([
    rdmFetch('/v1/ping'),
    new Promise((_, reject) => setTimeout(() => reject(new Error('Flash did not answer in time.')), ms)),
  ]);
}
