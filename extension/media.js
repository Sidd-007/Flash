// Which network responses are downloadable video, and how to download the
// video a player shows. Plain functions on top of common.js, used by the
// background worker and the tests.

/** Whole files smaller than this are previews, thumbnails or ads. */
const MIN_FILE_BYTES = 512 * 1024;

const header = (headers, name) => headers?.find((h) => h.name.toLowerCase() === name)?.value || '';

/** A stream manifest, by its URL alone. */
function manifestKind(url) {
  if (/\.m3u8(\?|#|$)/i.test(url)) return 'hls';
  if (/\.mpd(\?|#|$)/i.test(url)) return 'dash';
  return null;
}

/** The whole file's size: Content-Range's total ("bytes 0-1/12345"), else Content-Length. */
function responseSize(headers) {
  const range = header(headers, 'content-range').match(/\/(\d+)\s*$/);
  if (range) return Number(range[1]);
  const length = Number(header(headers, 'content-length'));
  return Number.isFinite(length) && length > 0 ? length : null;
}

/**
 * The media in a response (webRequest details), or null. Manifests count
 * however they were fetched. Whole files count only when a <video> or
 * <audio> element loaded them (type "media"): players that feed the element
 * themselves fetch small pieces with XHR, and those are covered by their
 * manifest instead.
 */
function classifyResponse(d) {
  if (!/^https?:/i.test(d.url) || d.statusCode >= 400) return null;
  const mime = header(d.responseHeaders, 'content-type').split(';')[0].trim().toLowerCase();
  let kind = manifestKind(d.url);
  if (/mpegurl/.test(mime)) kind = 'hls';
  else if (mime === 'application/dash+xml') kind = 'dash';
  if (kind) return { kind, url: d.url, mime, size: null };
  if (d.type !== 'media' || !/^(video|audio)\//.test(mime) || mime === 'video/mp2t') return null;
  const size = responseSize(d.responseHeaders);
  if (size !== null && size < MIN_FILE_BYTES) return null;
  return { kind: 'file', url: d.url, mime, size };
}

/**
 * The page yt-dlp should get for a video site, or null for other sites.
 * YouTube links are reduced to the video itself, so playlists and
 * timestamps don't come along.
 */
function videoPageUrl(url) {
  if (!url || !isVideoSite(url)) return null;
  let u;
  try { u = new URL(url); } catch { return null; }
  if (/(^|\.)(youtube\.com|youtube-nocookie\.com)$/.test(u.hostname)) {
    const v = u.searchParams.get('v');
    if (u.pathname === '/watch' && v) return `https://www.youtube.com/watch?v=${v}`;
    const m = u.pathname.match(/^\/(shorts|live|embed)\/([\w-]{6,})/);
    if (!m) return null;
    return m[1] === 'shorts' ? `https://www.youtube.com/shorts/${m[2]}` : `https://www.youtube.com/watch?v=${m[2]}`;
  }
  if (u.hostname === 'youtu.be' && u.pathname.length > 1) return `https://www.youtube.com/watch?v=${u.pathname.slice(1)}`;
  u.hash = '';
  return u.toString();
}

const hostOf = (url) => { try { return new URL(url).host; } catch { return ''; } };

function fmtSize(n) {
  if (!n) return '';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(i === 0 || n >= 100 ? 0 : 1)} ${units[i]}`;
}

function fileType(url, mime) {
  const ext = (url.split(/[?#]/)[0].match(/\.([a-z0-9]{2,4})$/i) || [])[1];
  if (ext) return ext.toUpperCase();
  return (mime || '').split('/')[1]?.toUpperCase() || 'Video';
}

/**
 * Ways to download the video a player shows, best first. Each is
 * `{ kind, url, label, size }` with `kind` as Flash takes it: "video"
 * (yt-dlp), "hls" (Flash's own stream downloader) or "file".
 *
 * - `frameUrl`: the page or frame the player is in; `pageUrl`: the tab.
 * - `src`: the element's current source (a `blob:` URL when the player
 *   feeds it pieces itself).
 * - `media`: what the network showed for the tab, oldest first; entries
 *   from the player's own frame are preferred.
 * - `hls`: what is known about HLS manifests by URL: `{ master, height }`.
 */
function videoChoices({ frameUrl, pageUrl, frameId = 0, src, media = [], hls = {} }) {
  // Sites yt-dlp knows get it: it finds the best quality and joins audio.
  const page = videoPageUrl(frameUrl) || videoPageUrl(pageUrl);
  if (page) return [{ kind: 'video', url: page, label: 'Best quality, via yt-dlp', size: null }];

  const choices = [];
  const seen = new Set();
  const add = (choice) => {
    if (seen.has(choice.url)) return;
    seen.add(choice.url);
    choices.push(choice);
  };
  const describe = (m) => {
    const where = hostOf(m.url);
    if (m.kind === 'hls') {
      const info = hls[m.url] || {};
      const quality = info.height ? `up to ${info.height}p` : info.master === false ? 'one quality' : '';
      return { kind: 'hls', url: m.url, label: ['HLS stream', quality, where].filter(Boolean).join(' · '), size: null };
    }
    if (m.kind === 'dash') return { kind: 'video', url: m.url, label: `DASH stream, via yt-dlp · ${where}`, size: null };
    return { kind: 'file', url: m.url, label: [fileType(m.url, m.mime), fmtSize(m.size), where].filter(Boolean).join(' · '), size: m.size };
  };

  // What the element itself plays, when it's a real address.
  if (/^https?:/i.test(src || '')) {
    const kind = manifestKind(src);
    add(describe({ kind: kind || 'file', url: src, size: media.find((m) => m.url === src)?.size }));
  }
  // Then what the network showed: this frame's first, newest first (the
  // main video usually loads after any ad); master playlists before single
  // qualities; streams before stray files.
  const rank = (m) => (m.kind === 'hls' ? (hls[m.url]?.master === false ? 1 : 0) : m.kind === 'dash' ? 2 : 3);
  const ordered = media
    .map((m, i) => ({ m, i }))
    .sort((a, b) => (b.m.frameId === frameId) - (a.m.frameId === frameId) || rank(a.m) - rank(b.m) || b.i - a.i)
    .map(({ m }) => m);
  for (const m of ordered) add(describe(m));

  // Nothing seen (say, it loaded before the extension did): yt-dlp can often
  // still find the video on the page.
  if (!choices.length && /^https?:/i.test(frameUrl || pageUrl || '')) {
    add({ kind: 'video', url: frameUrl || pageUrl, label: 'Let yt-dlp look for it', size: null });
  }
  return choices;
}

/** From an HLS playlist: whether it lists qualities, and the tallest one. */
function readHlsPlaylist(text) {
  const master = /#EXT-X-STREAM-INF/i.test(text);
  const heights = [...text.matchAll(/RESOLUTION=\d+x(\d+)/gi)].map((m) => Number(m[1]));
  return { master, height: heights.length ? Math.max(...heights) : null };
}

/** The tab title without the site's name tacked on. */
function cleanTitle(title) {
  return (title || '').replace(/^\(\d+\)\s*/, '').replace(/\s+[-|–—]\s+[^-|–—]{2,40}$/, '').trim();
}
