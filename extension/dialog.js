const $ = (sel) => document.querySelector(sel);
const storage = chrome.storage.session || chrome.storage.local;
const params = new URLSearchParams(location.search);
const id = params.get('id');
const key = `pending:${id}`;
/** Shown as an overlay inside the page rather than in its own window. */
const embedded = params.get('embedded') === '1';
let request = null;

function tellPage(type, extra = {}) {
  if (embedded) window.parent.postMessage({ rdmDialog: id, type, ...extra }, '*');
}

function fmtBytes(n) {
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(i === 0 || n >= 100 ? 0 : 1)} ${units[i]}`;
}

function nameFromUrl(url) {
  try {
    return decodeURIComponent(new URL(url).pathname.split('/').filter(Boolean).pop() || '') || 'download';
  } catch { return 'download'; }
}

async function finish() {
  await storage.remove(key);
  if (embedded) tellPage('close');
  else window.close();
}

const RECENT_KEY = 'recentDirs';

/** `/Users/me/Movies` -> `~/Movies`, for display. */
function tildify(path) {
  return request.home && path.startsWith(request.home) ? `~${path.slice(request.home.length)}` : path;
}

function folderName(path) {
  return path.split(/[\\/]/).filter(Boolean).pop() || path;
}

/** Windows paths use backslashes; keep to whichever the home folder uses. */
const isWindowsPath = (path) => /^[A-Za-z]:\\/.test(path || '');

/** Fills Where with: Flash's default, common folders, and recently used ones. */
async function fillFolders(selected) {
  const recent = (await chrome.storage.local.get(RECENT_KEY))[RECENT_KEY] || [];
  const home = request.home;
  const windows = isWindowsPath(home);
  const common = home
    ? (windows ? ['Desktop', 'Documents', 'Videos', 'Music'] : ['Desktop', 'Documents', 'Movies', 'Music'])
      .map((d) => `${home}${windows ? '\\' : '/'}${d}`)
    : [];
  const seen = new Set();
  const options = [];
  const add = (value, label) => {
    if (seen.has(value)) return;
    seen.add(value);
    options.push([value, label]);
  };
  // The empty value means "Flash's default", so its categories still apply.
  add('', request.download_dir ? `${folderName(request.download_dir)}  —  default` : 'Flash default');
  for (const p of recent) add(p, `${folderName(p)}  —  ${tildify(p)}`);
  for (const p of common) if (p !== request.download_dir) add(p, `${folderName(p)}  —  ${tildify(p)}`);
  if (selected && !seen.has(selected)) add(selected, `${folderName(selected)}  —  ${tildify(selected)}`);

  $('#dir').replaceChildren(...options.map(([value, label]) => {
    const o = document.createElement('option');
    o.value = value;
    o.textContent = label;
    return o;
  }));
  $('#dir').value = selected ?? '';
  $('#dir').title = $('#dir').value || request.download_dir || '';
}

/** The folder picked last time ('' = Flash's default), preselected next time. */
const LAST_KEY = 'lastDir';

async function rememberFolder(path) {
  await chrome.storage.local.set({ [LAST_KEY]: path || '' });
  if (!path) return;
  const recent = (await chrome.storage.local.get(RECENT_KEY))[RECENT_KEY] || [];
  await chrome.storage.local.set({ [RECENT_KEY]: [path, ...recent.filter((p) => p !== path)].slice(0, 5) });
}

/** Selects the name up to its extension, so typing replaces just the name. */
function selectStem(input, hasExtension) {
  const dot = hasExtension ? input.value.lastIndexOf('.') : -1;
  input.focus();
  // Backward keeps the start of a long name in view.
  input.setSelectionRange(0, dot > 0 ? dot : input.value.length, 'backward');
  input.scrollLeft = 0;
}

async function chooseFolder() {
  $('#choose').disabled = true;
  try {
    // Flash shows the system's folder picker and returns the path.
    const { path } = await rdmFetch('/v1/choose-folder', {
      method: 'POST',
      body: JSON.stringify({ start: $('#dir').value || request.download_dir }),
    });
    if (path) await fillFolders(path);
  } catch (e) {
    showError(e.message);
  } finally {
    $('#choose').disabled = false;
    window.focus();
  }
}

function showError(msg) {
  $('#error').textContent = msg;
  $('#error').hidden = false;
  tellHeight();
}

function tellHeight() {
  tellPage('height', { height: Math.ceil(document.documentElement.getBoundingClientRect().height) });
}

/** Warns when Flash already has this file (by link, or by name and size). */
async function checkDuplicates() {
  let found = [];
  try {
    ({ duplicates: found } = await rdmFetch('/v1/duplicates', {
      method: 'POST',
      body: JSON.stringify({ url: request.url, file_name: $('#name').value.trim() || null, size: request.size || null }),
    }));
  } catch { return; } // an older Flash without the check
  const d = found[0];
  const note = $('#dupe');
  note.hidden = !d;
  $('#ok').firstChild.textContent = d ? 'Download again' : 'Download';
  if (d) {
    const done = d.status === 'completed';
    const where = d.path ? tildify(d.path.replace(/\/[^/]+$/, '')) : '';
    const b = document.createElement('b');
    b.textContent = d.name;
    const extra = document.createElement('span');
    extra.className = 'muted';
    extra.textContent = [d.total ? fmtBytes(d.total) : '', where && `in ${where}`].filter(Boolean).map((t) => `  ·  ${t}`).join('');
    note.replaceChildren(done ? 'Already downloaded: ' : 'Already in Flash: ', b, extra);
  }
  tellHeight();
}

async function download() {
  $('#ok').disabled = true;
  $('#error').hidden = true;
  try {
    const dir = $('#dir').value || null;
    await sendToRdm(request.url, {
      referrer: request.referrer,
      file_name: $('#name').value.trim() || null,
      dir,
      start: $('#start').checked,
      confirmed: true,
      kind: request.kind || null,
      quality: request.kind === 'video' ? $('#quality').value : null,
    });
    await rememberFolder(dir);
    await finish();
  } catch (e) {
    showError(e.message);
    $('#ok').disabled = false;
  }
}

async function downloadInBrowser() {
  // Downloads started by this extension are never taken over again.
  await chrome.downloads.download({ url: request.url });
  await finish();
}

/** The name for a source: the page title plus the file's extension, if any. */
function nameFor(choice) {
  const ext = choice.kind === 'file' ? (choice.url.split(/[?#]/)[0].match(/\.([a-z0-9]{2,4})$/i) || [])[1] : null;
  if (!request.title) return choice.kind === 'file' ? nameFromUrl(choice.url) : '';
  return ext ? `${request.title}.${ext.toLowerCase()}` : request.title;
}

/**
 * Shows one way of downloading: yt-dlp ("video"), a stream ("hls") or a
 * file. Returns whether the name has an extension worth keeping selected.
 */
function showSource(choice, name) {
  Object.assign(request, { url: choice.url, kind: choice.kind, size: choice.size || null });
  const video = choice.kind === 'video';
  const stream = choice.kind === 'video' || choice.kind === 'hls';
  $('#name').value = name;
  const ext = (name.match(/\.([a-z0-9]{1,4})$/i) || [])[1];
  $('#ext').textContent = stream ? 'VID' : ext ? ext.toUpperCase() : 'FILE';
  for (const el of document.querySelectorAll('.video-only')) el.hidden = !video;
  $('#name').placeholder = stream ? 'the video title' : '';
  document.querySelector('.sheet-title').textContent = stream || request.choices ? 'Download video with Flash' : 'Download with Flash';
  // Plain links can fall back to the browser; pages and streams can't.
  $('#browser').hidden = stream;
  $('#error').hidden = true;
  if (video && !request.has_ytdlp) {
    showError(`Flash needs yt-dlp for this. Install it with: ${isWindowsPath(request.home) ? 'winget install yt-dlp.yt-dlp' : 'brew install yt-dlp'}`);
  }
  let host = '';
  try { host = new URL(choice.url).host; } catch { /* keep empty */ }
  const how = video ? 'via yt-dlp' : choice.kind === 'hls' ? 'HLS stream' : 'size unknown';
  $('#meta').textContent = [request.size ? fmtBytes(request.size) : how, host].filter(Boolean).join('  ·  ');
  $('#meta').title = choice.url;
  tellHeight();
  return !stream && !!ext;
}

async function main() {
  if (embedded) document.body.classList.add('embedded');
  request = (await storage.get(key))[key];
  if (!request) { finish(); return; }

  const name = request.file_name || (request.kind === 'video' ? '' : nameFromUrl(request.url));
  let keepExt = showSource({ url: request.url, kind: request.kind || 'file', size: request.size }, name);
  $('#quality').value = request.video_quality || 'best';
  const video = request.kind === 'video' || request.kind === 'hls';
  const ext = keepExt ? (name.match(/\.([a-z0-9]{1,4})$/i) || [])[1] : null;
  // A player can offer several sources; the best comes first.
  if (request.choices?.length > 1) {
    for (const el of document.querySelectorAll('.source-only')) el.hidden = false;
    $('#source').replaceChildren(...request.choices.map((c, i) => {
      const o = document.createElement('option');
      o.value = String(i);
      o.textContent = c.label;
      return o;
    }));
    $('#source').onchange = () => {
      const choice = request.choices[Number($('#source').value)];
      keepExt = showSource(choice, nameFor(choice));
    };
  }
  const last = (await chrome.storage.local.get(LAST_KEY))[LAST_KEY];
  await fillFolders(last || '');
  $('#dir').onchange = () => { $('#dir').title = $('#dir').value || request.download_dir || ''; };
  $('#choose').onclick = chooseFolder;

  $('#ok').onclick = download;
  $('#cancel').onclick = finish;
  $('#browser').onclick = downloadInBrowser;
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && e.target.tagName !== 'SELECT' && e.target.tagName !== 'BUTTON') { e.preventDefault(); download(); }
    if (e.key === 'Escape') finish();
  });
  // Selects the name (video titles have no extension yet; yt-dlp adds it),
  // unless the user has already clicked into something.
  const focusFirst = () => {
    if (document.activeElement && document.activeElement !== document.body) return;
    if (name) selectStem($('#name'), !video && !!ext && keepExt);
    else $('#ok').focus();
  };
  // Chrome ignores focus() in a frame from another origin until the page has
  // focused that frame, which it does once we say we're ready, and then tells
  // us so. The focus event covers the same moment where it fires.
  if (document.hasFocus()) focusFirst();
  window.addEventListener('focus', focusFirst, { once: true });
  window.addEventListener('message', (e) => {
    if (e.source === window.parent && e.data?.rdmDialog === id && e.data.type === 'focused') focusFirst();
  });
  tellPage('ready');
  tellHeight();
  checkDuplicates();
}

main();
