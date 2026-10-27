// Chrome loads this as a service worker; Firefox as a background script.
if (typeof importScripts === 'function') importScripts('common.js');

// ---------- taking over browser downloads ----------

const storage = chrome.storage.session || chrome.storage.local;

function fileNameOf(item) {
  const fromItem = (item.filename || '').split(/[\\/]/).pop();
  if (fromItem) return fromItem;
  try {
    return decodeURIComponent(new URL(item.finalUrl || item.url).pathname.split('/').pop() || '');
  } catch { return ''; }
}

function shouldCapture(item, s) {
  const url = item.finalUrl || item.url;
  if (!/^https?:/i.test(url)) return false; // blob:, data:, file:
  if (item.byExtensionId && item.byExtensionId === chrome.runtime.id) return false;
  if (s.captureAll) return true;

  const ext = (fileNameOf(item).match(/\.([a-z0-9]+)$/i) || [])[1]?.toLowerCase();
  const allowed = s.extensions.split(/[\s,]+/).filter(Boolean).map((e) => e.replace(/^\./, '').toLowerCase());
  if (!ext || !allowed.includes(ext)) return false;

  const size = item.totalBytes > 0 ? item.totalBytes : item.fileSize;
  if (s.minSizeMB > 0 && size > 0 && size < s.minSizeMB * 1024 * 1024) return false;
  return true;
}

/**
 * Takes a browser download over. `release` lets the browser carry on (Chrome:
 * the filename callback, which also brings up its Save As dialog if enabled).
 * We only cancel once Flash has answered, so nothing is lost if it's not running.
 */
async function takeOver(item, release) {
  const s = await getSettings();
  if (!s.enabled || !(await getToken())) return release();

  let info;
  if (!shouldCapture(item, s)) {
    // Not a file type Flash takes, unless Flash is waiting for a fresh link.
    info = /^https?:/i.test(item.finalUrl || item.url) ? await pingRdm(500).catch(() => null) : null;
    if (!info?.awaiting_link) return release();
  }
  try {
    info ??= await pingRdm();
  } catch (e) {
    release();
    notify('Downloading in the browser instead', e.message);
    return;
  }
  await chrome.downloads.cancel(item.id).catch(() => {});
  await chrome.downloads.erase({ id: item.id }).catch(() => {});

  const request = {
    url: item.finalUrl || item.url,
    referrer: item.referrer || null,
    file_name: fileNameOf(item) || null,
    size: item.totalBytes > 0 ? item.totalBytes : (item.fileSize > 0 ? item.fileSize : null),
    mime: item.mime || null,
  };
  // Flash is waiting for a fresh link to an unfinished download: no questions.
  if (s.askInBrowser && !info.awaiting_link) {
    await openDialog(request, info);
  } else {
    try {
      await sendToRdm(request.url, { referrer: request.referrer, file_name: request.file_name });
    } catch (e) {
      if (info.awaiting_link) {
        // Flash shows why the link didn't fit; the browser shouldn't start it.
        notify('Flash couldn’t use this link', e.message);
        return;
      }
      // Hand it back to the browser rather than losing it.
      chrome.downloads.download({ url: request.url });
      notify('Downloading in the browser instead', e.message);
    }
  }
}

/**
 * Shows Flash's download dialog as an overlay on the current page, like the
 * browser's own Save As sheet. Pages extensions can't touch (chrome://, the
 * Web Store, PDFs) get a small window instead.
 */
async function openDialog(request, info) {
  const id = crypto.randomUUID();
  await storage.set({ [`pending:${id}`]: {
    ...request,
    download_dir: info?.download_dir || null,
    home: info?.home || null,
    video_quality: info?.video_quality || 'best',
    has_ytdlp: info?.has_ytdlp ?? true,
  } });
  try {
    const [tab] = await chrome.tabs.query({ active: true, lastFocusedWindow: true });
    if (tab?.id != null && /^https?:/i.test(tab.url || '')) {
      await chrome.scripting.executeScript({
        target: { tabId: tab.id },
        func: showOverlay,
        args: [chrome.runtime.getURL(`dialog.html?id=${id}&embedded=1`), id, location.origin],
      });
      return;
    }
  } catch { /* restricted page: fall through */ }
  await openDialogWindow(id);
}

async function openDialogWindow(id) {
  const width = 500;
  const height = 270;
  let left;
  let top;
  try {
    const w = await chrome.windows.getLastFocused();
    left = Math.round(w.left + (w.width - width) / 2);
    top = Math.round(w.top + Math.max(60, (w.height - height) / 3));
  } catch { /* let the browser place it */ }
  await chrome.windows.create({ url: `dialog.html?id=${id}`, type: 'popup', width, height, left, top, focused: true });
}

// The overlay asks for a window when its frame can't load on that page.
chrome.runtime.onMessage.addListener((msg) => {
  if (msg?.type === 'rdm-overlay-failed' && typeof msg.id === 'string') openDialogWindow(msg.id);
});

/**
 * Runs inside the page (injected). Everything lives in a closed shadow root
 * and the dialog itself is an extension iframe, so the page can neither style
 * nor read it. Messages are only accepted from the extension's origin.
 */
function showOverlay(src, id, extensionOrigin) {
  document.getElementById('rdm-overlay-host')?.remove();
  const host = document.createElement('div');
  host.id = 'rdm-overlay-host';
  host.style.cssText = 'all: initial; position: fixed; inset: 0; z-index: 2147483647;';
  const root = host.attachShadow({ mode: 'closed' });
  root.innerHTML = `<style>
    .backdrop { position: fixed; inset: 0; display: flex; align-items: center; justify-content: center;
      background: rgba(0, 0, 0, 0.28); animation: fade 0.14s ease-out; }
    .card { width: 460px; max-width: calc(100vw - 32px); height: 214px; border-radius: 12px; overflow: hidden;
      box-shadow: 0 24px 80px rgba(0, 0, 0, 0.5), 0 0 0 0.5px rgba(128, 128, 128, 0.45);
      animation: pop 0.18s cubic-bezier(0.2, 0.9, 0.3, 1.15); }
    iframe { display: block; width: 100%; height: 100%; border: 0; }
    @keyframes fade { from { opacity: 0; } }
    @keyframes pop { from { opacity: 0; transform: translateY(-10px) scale(0.98); } }
  </style><div class="backdrop"><div class="card"><iframe title="Download with Flash"></iframe></div></div>`;
  const card = root.querySelector('.card');
  const frame = root.querySelector('iframe');
  let ready = false;

  const close = () => {
    window.removeEventListener('message', onMessage);
    host.remove();
  };
  function onMessage(e) {
    if (e.origin !== extensionOrigin || e.data?.rdmDialog !== id) return;
    if (e.data.type === 'ready') {
      ready = true;
      frame.focus();
    } else if (e.data.type === 'height') {
      card.style.height = `${Math.min(e.data.height, window.innerHeight - 32)}px`;
    } else if (e.data.type === 'close') {
      close();
    }
  }
  window.addEventListener('message', onMessage);
  frame.src = src;
  document.documentElement.appendChild(host);
  // Some pages block extension frames; fall back to a small window.
  setTimeout(() => {
    if (!ready) {
      close();
      chrome.runtime.sendMessage({ type: 'rdm-overlay-failed', id });
    }
  }, 3000);
}

if (chrome.downloads.onDeterminingFilename) {
  // Chrome: fires once the response headers are in, so name and size are
  // known, and before the Save As dialog. Holding `suggest` keeps Chrome
  // waiting while we check Flash; cancelling instead means no dialog at all.
  chrome.downloads.onDeterminingFilename.addListener((item, suggest) => {
    let released = false;
    const release = () => {
      if (!released) {
        released = true;
        suggest();
      }
    };
    takeOver(item, release).catch(release);
    return true;
  });
} else {
  // Firefox: the download is already running; leaving it alone lets it finish.
  chrome.downloads.onCreated.addListener((item) => takeOver(item, () => {}).catch(() => {}));
}

// Moves a token left in sync storage by an older version out of it.
getToken();

// The YouTube button only needs to know whether Flash is paired, not the token.
chrome.runtime.onMessage.addListener((msg, _sender, reply) => {
  if (msg?.type !== 'rdm-paired') return false;
  getToken().then((token) => reply(!!token));
  return true;
});

// ---------- right-click menu ----------

chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({ id: 'rdm-link', title: 'Download with Flash', contexts: ['link'] });
    chrome.contextMenus.create({ id: 'rdm-media', title: 'Download media with Flash', contexts: ['video', 'audio', 'image'] });
    chrome.contextMenus.create({
      id: 'rdm-page-video',
      title: 'Download video on this page with Flash',
      contexts: ['page', 'video'],
      documentUrlPatterns: VIDEO_SITES.flatMap((s) => [`*://${s}/*`, `*://*.${s}/*`]),
    });
  });
  getToken().then((token) => { if (!token) chrome.runtime.openOptionsPage(); });
});

/** Opens the dialog for the video on a video-site page (via yt-dlp). */
async function offerPageVideo(url, title) {
  try {
    const rdm = await pingRdm();
    await openDialog({ url, referrer: null, file_name: title || null, size: null, mime: null, kind: 'video' }, rdm);
  } catch (e) {
    notify('Could not send to Flash', e.message);
  }
}

chrome.runtime.onMessage.addListener((msg) => {
  if (msg?.type === 'rdm-page-video' && typeof msg.url === 'string') offerPageVideo(msg.url, msg.title);
});

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  if (info.menuItemId === 'rdm-page-video') {
    offerPageVideo(info.pageUrl, tab?.title?.replace(/ - YouTube$/, ''));
    return;
  }
  const url = info.linkUrl || info.srcUrl;
  if (!url || !/^https?:/i.test(url)) {
    notify('Can’t send this to Flash', 'Only http and https links can be downloaded.');
    return;
  }
  try {
    const rdm = await pingRdm();
    const s = await getSettings();
    if (s.askInBrowser) {
      await openDialog({ url, referrer: info.pageUrl || null, file_name: null, size: null, mime: null }, rdm);
    } else {
      await sendToRdm(url, { referrer: info.pageUrl || null });
    }
  } catch (e) {
    notify('Could not send to Flash', e.message);
  }
});

// ---------- HLS stream detection ----------

const streamsKey = (tabId) => `streams:${tabId}`;

async function rememberStream(tabId, url) {
  if (tabId < 0) return;
  const key = streamsKey(tabId);
  const list = (await storage.get(key))[key] || [];
  if (list.some((s) => s.url === url)) return;
  list.push({ url, at: Date.now() });
  await storage.set({ [key]: list.slice(-30) });
  chrome.action.setBadgeText({ tabId, text: String(Math.min(list.length, 99)) });
  chrome.action.setBadgeBackgroundColor({ tabId, color: '#0a64e0' });
}

async function forgetStreams(tabId) {
  await storage.remove(streamsKey(tabId));
  chrome.action.setBadgeText({ tabId, text: '' }).catch(() => {});
}

chrome.webRequest.onBeforeRequest.addListener(
  (d) => { if (/\.m3u8(\?|#|$)/i.test(d.url)) rememberStream(d.tabId, d.url); },
  { urls: ['<all_urls>'] },
);

chrome.webRequest.onHeadersReceived.addListener(
  (d) => {
    const type = d.responseHeaders?.find((h) => h.name.toLowerCase() === 'content-type')?.value || '';
    if (/mpegurl/i.test(type)) rememberStream(d.tabId, d.url);
  },
  { urls: ['<all_urls>'] },
  ['responseHeaders'],
);

chrome.tabs.onUpdated.addListener((tabId, change) => { if (change.url) forgetStreams(tabId); });
chrome.tabs.onRemoved.addListener((tabId) => forgetStreams(tabId));

// ---------- helpers ----------

function notify(title, message) {
  chrome.notifications.create({ type: 'basic', iconUrl: 'icons/128.png', title, message }).catch?.(() => {});
}
