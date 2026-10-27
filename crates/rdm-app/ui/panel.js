// The menu bar panel: live downloads, speed profiles, and a way into the app.

const { invoke } = window.__TAURI__.core;
const { listen, emit } = window.__TAURI__.event;

const $ = (sel) => document.querySelector(sel);
const items = new Map();
let profile = 'full';
let settings = null;

const ICONS = {
  pause: '<svg viewBox="0 0 16 16"><path d="M5.5 4v8M10.5 4v8"/></svg>',
  play: '<svg viewBox="0 0 16 16"><path d="M5 3.5v9l7-4.5z"/></svg>',
  retry: '<svg viewBox="0 0 16 16"><path d="M13 8a5 5 0 1 1-1.5-3.55M13 3v3h-3"/></svg>',
  reveal: '<svg viewBox="0 0 16 16"><path d="M2.5 4.5h4l1.5 1.5h5.5v6.5h-11z"/></svg>',
};
const STATE_LABEL = { running: 'Live', queued: 'Queued', paused: 'Paused', completed: 'Done', failed: 'Failed' };

function fmtBytes(n) {
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(i === 0 || n >= 100 ? 0 : 1)} ${units[i]}`;
}

const fmtSpeed = (n) => `${fmtBytes(n)}/s`;

function fmtEta(d) {
  if (!d.total || !d.speed) return '';
  const s = Math.round((d.total - d.downloaded) / d.speed);
  if (s < 60) return `${s}s left`;
  if (s < 3600) return `${Math.round(s / 60)}m left`;
  return `${Math.floor(s / 3600)}h ${Math.round((s % 3600) / 60)}m left`;
}

function nameOf(d) {
  if (d.file_name) return d.file_name;
  try {
    return decodeURIComponent(new URL(d.url).pathname.split('/').filter(Boolean).pop() || d.url);
  } catch { return d.url; }
}

const LIVE = ['running', 'queued', 'paused'];

/** Running first, then the newest; finished ones only when nothing is live. */
function visible() {
  const all = [...items.values()];
  const live = all.filter((d) => LIVE.includes(d.status))
    .sort((a, b) => (b.status === 'running') - (a.status === 'running') || b.id - a.id);
  if (live.length) return { live: true, rows: live.slice(0, 6) };
  const recent = all.filter((d) => d.status === 'completed' || d.status === 'failed')
    .sort((a, b) => (b.finished_at || b.added_at) - (a.finished_at || a.added_at));
  return { live: false, rows: recent.slice(0, 4) };
}

function row(d) {
  const li = document.createElement('li');
  li.className = `mb-row ${d.status}`;
  const top = document.createElement('div');
  top.className = 'mb-top';
  const name = document.createElement('span');
  name.className = 'mb-name';
  name.textContent = nameOf(d);
  name.title = nameOf(d);
  top.append(name);

  const action = d.status === 'running' || d.status === 'queued' ? ['pause', 'Pause', 'pause']
    : d.status === 'paused' ? ['play', 'Resume', 'resume']
    : d.status === 'failed' ? ['retry', 'Retry', 'resume']
    : d.path ? ['reveal', `Show in ${FILE_MANAGER}`, 'reveal_download'] : null;
  if (action) {
    const b = document.createElement('button');
    b.type = 'button';
    b.title = action[1];
    b.innerHTML = ICONS[action[0]];
    b.onclick = () => invoke(action[2], { id: d.id }).catch(() => {});
    top.append(b);
  }

  const bar = document.createElement('div');
  bar.className = 'mb-bar';
  const fill = document.createElement('i');
  const f = d.total ? Math.min(1, d.downloaded / d.total) : null;
  fill.style.width = `${((f ?? 0) * 100).toFixed(2)}%`;
  bar.classList.toggle('indeterminate', f == null && d.status === 'running');
  bar.append(fill);

  const meta = document.createElement('div');
  meta.className = 'mb-meta';
  const left = document.createElement('span');
  left.textContent = d.status === 'running'
    ? [f != null ? `${Math.floor(f * 100)}%` : fmtBytes(d.downloaded), fmtSpeed(d.speed), fmtEta(d)].filter(Boolean).join('  ·  ')
    : d.status === 'completed' ? fmtBytes(d.total ?? d.downloaded)
    : d.total ? `${fmtBytes(d.downloaded)} of ${fmtBytes(d.total)}` : fmtBytes(d.downloaded);
  if (d.status === 'failed' && d.error) left.title = d.error;
  const st = document.createElement('span');
  st.className = 'st';
  st.textContent = d.status === 'queued' && d.retry_at > Date.now() ? 'Retry' : STATE_LABEL[d.status] || d.status;
  meta.append(left, st);

  li.append(top, bar, meta);
  return li;
}

function render() {
  const all = [...items.values()];
  const running = all.filter((d) => d.status === 'running');
  const queued = all.filter((d) => d.status === 'queued').length;
  const speed = running.reduce((sum, d) => sum + (d.speed || 0), 0);
  $('#mb-speed').textContent = running.length ? fmtSpeed(speed) : 'idle';
  $('#mb-speed').classList.toggle('idle', !running.length);
  $('#mb-count').textContent = [running.length && `${running.length} live`, queued && `${queued} queued`, settings?.speed_limit && `cap ${fmtSpeed(settings.speed_limit)}`]
    .filter(Boolean).join('  ·  ');

  const { live, rows } = visible();
  $('#mb-label').textContent = live ? 'Downloading' : 'Recent';
  $('#mb-label').classList.toggle('hidden', !rows.length);
  $('#mb-list').replaceChildren(...rows.map(row));
  $('#mb-empty').classList.toggle('hidden', rows.length > 0);

  for (const b of document.querySelectorAll('#mb-profiles button')) {
    const on = b.dataset.profile === profile;
    b.classList.toggle('active', on);
    b.setAttribute('aria-checked', String(on));
  }
  if (settings) $('#mb-bg').title = `All downloads capped at ${fmtSpeed(settings.background_limit)}`;
}

async function reload() {
  const [list, p, s] = await Promise.all([invoke('list_downloads'), invoke('get_profile'), invoke('get_settings')]);
  items.clear();
  for (const d of list) items.set(d.id, d);
  profile = p;
  settings = s;
  applyTheme(s.theme);
  render();
}

function onEvent(e) {
  switch (e.type) {
    case 'changed':
      items.set(e.download.id, e.download);
      break;
    case 'removed':
      items.delete(e.id);
      break;
    case 'progress':
      for (const p of e.items) {
        const d = items.get(p.id);
        if (d) Object.assign(d, { downloaded: p.downloaded, total: p.total ?? d.total, speed: p.speed });
      }
      break;
    default:
      return;
  }
  render();
}

async function main() {
  for (const b of document.querySelectorAll('#mb-profiles button')) {
    b.onclick = () => invoke('set_profile', { profile: b.dataset.profile }).catch(() => {});
  }
  $('#mb-open').onclick = () => invoke('show_main_window');
  $('#mb-add').onclick = async () => {
    await invoke('show_main_window');
    await emit('rdm-show-add');
  };
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') window.__TAURI__.window.getCurrentWindow().hide();
  });
  // The panel stays loaded while hidden; catch up whenever it opens.
  window.addEventListener('focus', reload);
  await listen('rdm-event', (e) => onEvent(e.payload));
  await listen('rdm-profile', (e) => { profile = e.payload; render(); });
  await listen('rdm-settings-changed', async () => {
    settings = await invoke('get_settings');
    applyTheme(settings.theme);
    render();
  });
  await reload();
}

main();
