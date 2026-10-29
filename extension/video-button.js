// Puts a "Download" button on video players on any site, like IDM's. It
// shows while the pointer is over a player (and briefly when one starts).
// Clicking it asks the background worker to work out how to download what
// the player shows (see media.js) and to open Flash's dialog.

(() => {
  // Each frame gets its own copy; never two in one frame.
  if (window.__flashVideoButton) return;
  window.__flashVideoButton = true;

  /** Players smaller than this are thumbnails and previews. */
  const MIN_WIDTH = 200;
  const MIN_HEIGHT = 120;
  /** How long the button stays after the pointer leaves the player. */
  const LINGER_MS = 1500;
  /** How long it shows when a player starts on its own. */
  const ON_PLAY_MS = 2500;

  // Built without innerHTML: some sites (YouTube) enforce Trusted Types.
  const CSS = `
    :host { all: initial; position: fixed; z-index: 2147483646; }
    .pill { display: flex; align-items: stretch; height: 30px; border-radius: 8px; overflow: hidden;
      background: rgba(10, 10, 10, 0.82); color: #fafafa; backdrop-filter: blur(8px);
      box-shadow: 0 0 0 0.5px rgba(255, 255, 255, 0.22), 0 6px 20px rgba(0, 0, 0, 0.35);
      font: 500 12px/1 ui-monospace, "SF Mono", "Cascadia Mono", Consolas, Menlo, monospace; letter-spacing: 0.01em;
      transition: opacity 0.16s ease, transform 0.16s ease; }
    .pill.hidden { opacity: 0; transform: translateY(-4px); pointer-events: none; }
    button { all: unset; display: flex; align-items: center; gap: 7px; padding: 0 11px; cursor: pointer; }
    button:hover { background: rgba(255, 255, 255, 0.12); }
    button:focus-visible { outline: 1.5px solid #fafafa; outline-offset: -3px; }
    svg { width: 14px; height: 14px; }
    .brand { color: rgba(250, 250, 250, 0.55); }
    .close { padding: 0 9px; border-left: 0.5px solid rgba(255, 255, 255, 0.18); color: rgba(250, 250, 250, 0.6); }
    .close:hover { color: #fafafa; }`;

  function el(tag, className, attrs = {}, text = '') {
    const node = document.createElement(tag);
    if (className) node.className = className;
    for (const [k, v] of Object.entries(attrs)) node.setAttribute(k, v);
    if (text) node.textContent = text;
    return node;
  }

  function icon() {
    const ns = 'http://www.w3.org/2000/svg';
    const svg = document.createElementNS(ns, 'svg');
    svg.setAttribute('viewBox', '0 0 16 16');
    svg.setAttribute('aria-hidden', 'true');
    const path = document.createElementNS(ns, 'path');
    for (const [k, v] of Object.entries({ d: 'M8 2.5v8m0 0L4.75 7.25M8 10.5l3.25-3.25M3 13.5h10', fill: 'none', stroke: 'currentColor',
      'stroke-width': '1.6', 'stroke-linecap': 'round', 'stroke-linejoin': 'round' })) path.setAttribute(k, v);
    svg.append(path);
    return svg;
  }

  /** Every <video> in this frame, kept up to date by the browser. */
  const videos = document.getElementsByTagName('video');

  let enabled = false;
  let host = null;
  let pill = null;
  /** The player the button is on. */
  let current = null;
  let hideTimer = 0;
  let overPill = false;
  /** The page the user closed the button on; it comes back on the next one. */
  let dismissed = null;

  function build() {
    host = el('div');
    const root = host.attachShadow({ mode: 'closed' });
    const style = el('style');
    style.textContent = CSS;
    pill = el('div', 'pill hidden');
    const get = el('button', 'get', { title: 'Download this video with Flash' });
    get.append(icon(), el('span', '', {}, 'Download'), el('span', 'brand', {}, 'flash'));
    const close = el('button', 'close', { title: 'Hide on this page', 'aria-label': 'Hide' }, '✕');
    pill.append(get, close);
    root.append(style, pill);
    // Keep clicks away from the page, which would pause, seek or navigate.
    for (const type of ['click', 'mousedown', 'mouseup', 'pointerdown', 'pointerup', 'dblclick']) {
      host.addEventListener(type, (e) => e.stopPropagation());
    }
    host.addEventListener('pointerenter', () => { overPill = true; clearTimeout(hideTimer); });
    host.addEventListener('pointerleave', () => { overPill = false; hideSoon(LINGER_MS); });
    get.addEventListener('click', download);
    close.addEventListener('click', () => {
      dismissed = location.href;
      hide();
    });
  }

  /** Big enough, on screen, playing a real source, and not protected. */
  function eligible(video) {
    // A camera or call (srcObject), or DRM-protected video (mediaKeys).
    if (video.srcObject || video.mediaKeys) return false;
    if (!(video.currentSrc || video.src || video.querySelector('source'))) return false;
    const r = video.getBoundingClientRect();
    return r.width >= MIN_WIDTH && r.height >= MIN_HEIGHT
      && r.bottom > 0 && r.right > 0 && r.top < innerHeight && r.left < innerWidth;
  }

  /** The topmost eligible player under a point. */
  function videoAt(x, y) {
    let found = null;
    for (const video of videos) {
      if (!eligible(video)) continue;
      const r = video.getBoundingClientRect();
      if (x >= r.left && x <= r.right && y >= r.top && y <= r.bottom) found = video;
    }
    return found;
  }

  /** Top-right corner of the player, inside the viewport. */
  function place() {
    if (!current?.isConnected || !eligible(current)) return hide();
    const r = current.getBoundingClientRect();
    // YouTube Shorts keep play, volume and ⋮ along the top edge.
    const top = /(^|\.)youtube\.com$/.test(location.hostname) && location.pathname.startsWith('/shorts/') ? 60 : 10;
    const width = pill.getBoundingClientRect().width || 170;
    host.style.left = `${Math.max(4, Math.min(r.right - width - 10, innerWidth - width - 4))}px`;
    host.style.top = `${Math.max(4, Math.min(r.top + top, innerHeight - 40))}px`;
  }

  /** Shows the button on a player, following it as the page scrolls. */
  function show(video) {
    if (!host) build();
    // In fullscreen only the fullscreen element is drawn, so go inside it.
    const parent = document.fullscreenElement && document.fullscreenElement !== video
      ? document.fullscreenElement
      : document.documentElement;
    if (host.parentNode !== parent) parent.appendChild(host);
    current = video;
    clearTimeout(hideTimer);
    pill.classList.remove('hidden');
    place();
  }

  function hide() {
    clearTimeout(hideTimer);
    pill?.classList.add('hidden');
    current = null;
  }

  function hideSoon(ms) {
    clearTimeout(hideTimer);
    hideTimer = setTimeout(() => { if (!overPill) hide(); }, ms);
  }

  const active = () => enabled && dismissed !== location.href && youtubeVideoPage();

  async function download() {
    const video = current;
    if (!video) return;
    // The dialog opens over the page, which is hidden while in fullscreen.
    if (document.fullscreenElement) await document.exitFullscreen().catch(() => {});
    video.pause();
    hide();
    chrome.runtime.sendMessage({ type: 'rdm-video', src: video.currentSrc || video.src || '' }).catch(() => {});
  }

  // Follow the pointer, checking at most once a frame, and only on pages
  // that have a video at all.
  let pending = null;
  document.addEventListener('pointermove', (e) => {
    if (!active() || !videos.length) return;
    if (!pending) {
      requestAnimationFrame(() => {
        const { x, y } = pending;
        pending = null;
        const video = videoAt(x, y);
        if (video) show(video);
        else if (current && !overPill) hideSoon(LINGER_MS);
      });
    }
    pending = { x: e.clientX, y: e.clientY };
  }, { capture: true, passive: true });

  // A player starting on its own shows the button briefly, so it's found.
  document.addEventListener('play', (e) => {
    if (active() && e.target instanceof HTMLVideoElement && eligible(e.target) && !current) {
      show(e.target);
      hideSoon(ON_PLAY_MS);
    }
  }, true);

  for (const type of ['scroll', 'resize', 'fullscreenchange']) {
    addEventListener(type, () => { if (current) (type === 'fullscreenchange' ? show(current) : place()); }, { capture: true, passive: true });
  }

  /** The site the user sees: the top page's host, even inside an embedded player. */
  function siteHost() {
    const origins = location.ancestorOrigins;
    try {
      return origins?.length ? new URL(origins[origins.length - 1]).hostname : location.hostname;
    } catch { return location.hostname; }
  }

  /** On YouTube itself, only real video pages: not thumbnails' hover previews. */
  function youtubeVideoPage() {
    if (!/(^|\.)(youtube\.com|youtube-nocookie\.com)$/.test(location.hostname)) return true;
    return /^\/(watch|shorts\/|live\/|embed\/)/.test(location.pathname);
  }

  async function load() {
    const s = await getSettings();
    const paired = await chrome.runtime.sendMessage({ type: 'rdm-paired' }).catch(() => false);
    const site = siteHost().replace(/^www\./, '');
    const excluded = s.videoButtonExclude.split(/[\s,]+/).filter(Boolean)
      .some((h) => site === h.replace(/^www\./, '') || site.endsWith(`.${h.replace(/^www\./, '')}`));
    enabled = s.videoButton && paired && !excluded;
    if (!enabled) hide();
  }

  // Single-page sites (YouTube) change pages without reloading.
  const sync = () => { if (!active()) hide(); };
  document.addEventListener('yt-navigate-finish', sync);
  addEventListener('popstate', sync);

  chrome.storage.onChanged.addListener(() => load());
  load();
})();
