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
  return path.split('/').filter(Boolean).pop() || path;
}

/** Fills Where with: Flash's default, common folders, and recently used ones. */
async function fillFolders(selected) {
  const recent = (await chrome.storage.local.get(RECENT_KEY))[RECENT_KEY] || [];
  const home = request.home;
  const common = home ? ['Desktop', 'Documents', 'Movies', 'Music'].map((d) => `${home}/${d}`) : [];
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
    // Flash shows the native macOS folder picker and returns the path.
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

async function main() {
  if (embedded) document.body.classList.add('embedded');
  request = (await storage.get(key))[key];
  if (!request) { finish(); return; }

  const video = request.kind === 'video';
  const name = request.file_name || (video ? '' : nameFromUrl(request.url));
  $('#name').value = name;
  const ext = (name.match(/\.([a-z0-9]{1,4})$/i) || [])[1];
  $('#ext').textContent = video ? 'VID' : ext ? ext.toUpperCase() : 'FILE';
  if (video) {
    for (const el of document.querySelectorAll('.video-only')) el.hidden = false;
    $('#quality').value = request.video_quality || 'best';
    $('#name').placeholder = 'the video title';
    document.querySelector('.sheet-title').textContent = 'Download video with Flash';
    // Plain links can fall back to Chrome; video pages can't.
    $('#browser').hidden = true;
    if (!request.has_ytdlp) showError('Flash needs yt-dlp for video sites. In Terminal: brew install yt-dlp');
  }
  let host = '';
  try { host = new URL(request.url).host; } catch { /* keep empty */ }
  $('#meta').textContent = [request.size ? fmtBytes(request.size) : video ? 'via yt-dlp' : 'size unknown', host].filter(Boolean).join('  ·  ');
  $('#meta').title = request.url;
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
  tellPage('ready');
  tellHeight();
  checkDuplicates();
  // Video titles have no extension yet; yt-dlp adds it.
  if (name) selectStem($('#name'), !video && !!ext);
  else $('#ok').focus();
}

main();
