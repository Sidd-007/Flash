// Puts a "Download" button on YouTube's player, like IDM's. Clicking it opens
// RDM's download dialog for the video (through yt-dlp). YouTube swaps pages
// without reloading, so the button follows whichever player is showing.

(() => {
  // Built without innerHTML: YouTube enforces Trusted Types.
  const CSS = `
    .pill { display: flex; align-items: stretch; height: 30px; border-radius: 8px; overflow: hidden;
      background: rgba(10, 10, 10, 0.82); color: #fafafa; backdrop-filter: blur(8px);
      box-shadow: 0 0 0 0.5px rgba(255, 255, 255, 0.22), 0 6px 20px rgba(0, 0, 0, 0.35);
      font: 500 12px/1 ui-monospace, "SF Mono", Menlo, monospace; letter-spacing: 0.01em;
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

  let host = null;
  let button = null;
  let player = null;
  /** Video the user closed the button on; it comes back on the next one. */
  let dismissed = null;
  let wanted = true;

  /** The watch URL yt-dlp should get, or null when no single video is showing. */
  function videoUrl() {
    const { pathname, searchParams } = new URL(location.href);
    if (pathname === '/watch' && searchParams.get('v')) return `https://www.youtube.com/watch?v=${searchParams.get('v')}`;
    const m = pathname.match(/^\/(shorts|live|embed)\/([\w-]{6,})/);
    if (!m) return null;
    return m[1] === 'shorts' ? `https://www.youtube.com/shorts/${m[2]}` : `https://www.youtube.com/watch?v=${m[2]}`;
  }

  function videoTitle() {
    const text = (node) => node?.textContent?.trim() || '';
    return text(document.querySelector('ytd-watch-metadata h1'))
      || text(document.querySelector('ytd-reel-video-renderer[is-active] h2'))
      || text(document.querySelector('.ytp-title-link'))
      || document.title.replace(/^\(\d+\)\s*/, '').replace(/ - YouTube$/, '');
  }

  function findPlayer() {
    if (location.pathname.startsWith('/shorts/')) {
      return document.querySelector('ytd-reel-video-renderer[is-active] .html5-video-player')
        || document.querySelector('#shorts-player');
    }
    return document.querySelector('#movie_player') || document.querySelector('.html5-video-player');
  }

  function build() {
    host = document.createElement('div');
    host.style.cssText = 'all: initial; position: absolute; top: 12px; right: 12px; z-index: 70;';
    const root = host.attachShadow({ mode: 'closed' });
    const style = document.createElement('style');
    style.textContent = CSS;
    button = el('div', 'pill hidden');
    const get = el('button', 'get', { title: 'Download this video with RDM' });
    get.append(icon(), el('span', '', {}, 'Download'), el('span', 'brand', {}, 'rdm'));
    const close = el('button', 'close', { title: 'Hide for this video', 'aria-label': 'Hide' }, '✕');
    button.append(get, close);
    root.append(style, button);
    // Keep clicks away from the player, which would pause or seek.
    for (const type of ['click', 'mousedown', 'pointerdown', 'dblclick']) {
      host.addEventListener(type, (e) => e.stopPropagation());
    }
    get.addEventListener('click', download);
    close.addEventListener('click', () => {
      dismissed = videoUrl();
      update();
    });
  }

  async function download() {
    const url = videoUrl();
    if (!url) return;
    // The dialog opens over the page, which is hidden while the player is fullscreen.
    if (document.fullscreenElement) await document.exitFullscreen().catch(() => {});
    player?.querySelector('video')?.pause();
    chrome.runtime.sendMessage({ type: 'rdm-page-video', url, title: videoTitle() });
  }

  /** Shows the button like YouTube shows its own controls: on hover or when paused. */
  function syncVisibility() {
    if (!player || !button) return;
    const idle = player.classList.contains('ytp-autohide') && !player.classList.contains('paused-mode');
    button.classList.toggle('hidden', idle || player.classList.contains('ad-showing'));
  }

  const classWatcher = new MutationObserver(syncVisibility);

  function update() {
    const url = videoUrl();
    const next = wanted && url && url !== dismissed ? findPlayer() : null;
    if (next !== player) {
      classWatcher.disconnect();
      host?.remove();
      player = next;
      if (player) {
        if (!host) build();
        // Shorts keep play, volume and ⋮ along the top edge; sit below them.
        host.style.top = location.pathname.startsWith('/shorts/') ? '60px' : '12px';
        player.appendChild(host);
        classWatcher.observe(player, { attributes: true, attributeFilter: ['class'] });
      }
    }
    syncVisibility();
  }

  async function loadSetting() {
    const s = await getSettings();
    wanted = s.videoButton && !!s.token;
    update();
  }

  chrome.storage.onChanged.addListener((_, area) => { if (area === 'sync') loadSetting(); });
  document.addEventListener('yt-navigate-finish', update);
  // The player is created late and Shorts swap it while scrolling. YouTube
  // changes the page constantly, so check at most once a frame.
  let queued = false;
  new MutationObserver(() => {
    if (queued || (player?.isConnected && !location.pathname.startsWith('/shorts/'))) return;
    queued = true;
    requestAnimationFrame(() => { queued = false; update(); });
  }).observe(document.documentElement, { childList: true, subtree: true });
  loadSetting();
})();
