'use strict';

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (sel) => document.querySelector(sel);
const $$ = (sel) => [...document.querySelectorAll(sel)];

const state = {
  items: new Map(),          // id -> download view
  rows: new Map(),           // id -> <tr>
  visible: [],               // ids in display order
  filter: 'all',
  category: null,
  search: '',
  selected: new Set(),
  anchor: null,              // for shift-click ranges
  settings: null,
  totalSpeed: 0,
  pending: null,             // headers/kind from a browser hand-off
  segments: new Map(),       // id -> missing ranges, for the map
  speeds: [],                // recent total speeds, for the sparkline
  view: 'list',              // 'list' or 'stats'
  typesOpen: loadPref('typesOpen', false),
};

/** Small per-machine UI preferences; storage may be unavailable. */
function loadPref(key, fallback) {
  try {
    const v = localStorage.getItem(`rdm.${key}`);
    return v === null ? fallback : JSON.parse(v);
  } catch { return fallback; }
}

function savePref(key, value) {
  try { localStorage.setItem(`rdm.${key}`, JSON.stringify(value)); } catch { /* not persisted */ }
}

const STATUS_FILTERS = [
  ['all', 'All', () => true],
  ['active', 'Active', (d) => d.status === 'running' || d.status === 'queued'],
  ['paused', 'Paused', (d) => d.status === 'paused'],
  ['completed', 'Done', (d) => d.status === 'completed'],
  ['failed', 'Failed', (d) => d.status === 'failed'],
];

const STATE_LABEL = { running: 'Live', queued: 'Queued', paused: 'Paused', completed: 'Done', failed: 'Failed' };

const ICONS = {
  pause: '<svg viewBox="0 0 16 16"><path d="M5.5 4v8M10.5 4v8"/></svg>',
  play: '<svg viewBox="0 0 16 16"><path d="M5 3.8v8.4L12 8z" fill="currentColor" stroke="none"/></svg>',
  reveal: '<svg viewBox="0 0 16 16"><path d="M2.5 4.5h4l1.5 1.5h5.5v6.5h-11z"/></svg>',
  retry: '<svg viewBox="0 0 16 16"><path d="M12.5 8A4.5 4.5 0 1 1 11 4.6M12.5 2.5v2.5H10"/></svg>',
  remove: '<svg viewBox="0 0 16 16"><path d="M4 4l8 8M12 4l-8 8"/></svg>',
};

// ---------- formatting ----------

function fmtBytes(n) {
  if (n == null || isNaN(n)) return '—';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(i === 0 || n >= 100 ? 0 : 1)} ${units[i]}`;
}

function fmtSpeed(n) {
  return n > 0 ? `${fmtBytes(n)}/s` : '';
}

function speedParts(n) {
  if (!(n > 0)) return ['0', 'B/s'];
  const [num, unit] = fmtBytes(n).split(' ');
  return [num, `${unit}/s`];
}

function hostOf(url) {
  try { return new URL(url).host; } catch { return ''; }
}

function fmtDuration(s) {
  if (!isFinite(s) || s < 0) return '';
  s = Math.round(s);
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s`;
  return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
}

function fmtDate(ms) {
  const d = new Date(ms);
  const now = new Date();
  if (d.toDateString() === now.toDateString()) {
    return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  }
  return d.toLocaleDateString([], { month: 'short', day: 'numeric' });
}

/** "2M" -> bytes/s. '' -> 0 (unlimited). Invalid -> NaN. */
function parseRate(s) {
  s = String(s || '').trim().toUpperCase().replace(/\/S$/, '').replace(/B$/, '');
  if (!s) return 0;
  const m = s.match(/^([\d.]+)\s*([KMG]?)$/);
  if (!m) return NaN;
  const mult = { '': 1, K: 1024, M: 1024 ** 2, G: 1024 ** 3 }[m[2]];
  return Math.floor(parseFloat(m[1]) * mult);
}

function fmtRate(n) {
  if (!n) return '';
  if (n % (1024 ** 2) === 0) return `${n / 1024 ** 2}M`;
  if (n % 1024 === 0) return `${n / 1024}K`;
  return String(n);
}

function nameOf(d) {
  if (d.file_name) return d.file_name;
  try {
    const parts = new URL(d.url).pathname.split('/').filter(Boolean);
    return decodeURIComponent(parts.pop() || d.url);
  } catch { return d.url; }
}

function extOf(name) {
  const m = name.match(/\.([a-z0-9]{1,5})$/i);
  return m ? m[1].toUpperCase() : '';
}

/** Fraction 0..1, or null if unknown. */
function fraction(d) {
  if (d.status === 'completed') return 1;
  if (d.parts) return d.parts[1] ? d.parts[0] / d.parts[1] : null;
  if (d.total) return Math.min(1, d.downloaded / d.total);
  return null;
}

function scheduleBlocks(d) {
  const s = state.settings?.schedule;
  if (!s?.enabled || d.manual) return false;
  const now = new Date();
  const mins = now.getHours() * 60 + now.getMinutes();
  const [sh, sm] = s.start.split(':').map(Number);
  const [eh, em] = s.stop.split(':').map(Number);
  const start = sh * 60 + sm, stop = eh * 60 + em;
  const open = start < stop ? mins >= start && mins < stop : mins >= start || mins < stop;
  return !open;
}

function statusText(d) {
  if (d.status === 'queued' && d.retry_at && d.retry_at > Date.now()) {
    return `network problem · retry ${d.auto_retries} in ${fmtDuration((d.retry_at - Date.now()) / 1000)}`;
  }
  switch (d.status) {
    case 'running': {
      if (d.parts && d.parts[0] > 0) return `segment ${d.parts[0]}/${d.parts[1]}`;
      if (d.total && d.speed > 0) return `${fmtDuration((d.total - d.downloaded) / d.speed)} left`;
      return 'connecting';
    }
    case 'queued': return scheduleBlocks(d) ? `waits for ${state.settings.schedule.start}` : 'queued';
    case 'paused': return 'paused';
    case 'completed': return d.finished_at ? `done ${fmtDate(d.finished_at)}` : 'done';
    case 'failed': return d.error ? `failed · ${d.error}` : 'failed';
    default: return d.status;
  }
}

// ---------- list ----------

function matches(d) {
  const filter = STATUS_FILTERS.find(([k]) => k === state.filter);
  if (filter && !filter[2](d)) return false;
  if (state.category) {
    const cat = d.category || 'Other';
    if (cat !== state.category) return false;
  }
  if (state.search) {
    const q = state.search.toLowerCase();
    if (!nameOf(d).toLowerCase().includes(q) && !d.url.toLowerCase().includes(q)) return false;
  }
  return true;
}

function makeRow(id) {
  const li = document.createElement('li');
  li.className = 'row';
  li.dataset.id = id;
  li.innerHTML = `
    <div class="glyph"></div>
    <div class="main"><div class="name"></div><div class="meta"></div></div>
    <div class="speed"></div>
    <div class="pct"></div>
    <div class="state"></div>
    <div class="bar"><i></i></div>
    <div class="actions"></div>`;
  li.addEventListener('mousedown', (e) => { if (!e.target.closest('.actions')) onRowClick(e, id); });
  li.addEventListener('dblclick', (e) => { if (!e.target.closest('.actions')) onRowDouble(id); });
  li.addEventListener('contextmenu', (e) => onRowMenu(e, id));
  return li;
}

function rowActions(d) {
  const acts = [];
  if (d.status === 'running' || d.status === 'queued') acts.push(['pause', 'Pause', () => act('pause', d.id)]);
  if (d.status === 'paused') acts.push(['play', 'Resume', () => act('resume', d.id)]);
  if (d.status === 'failed') acts.push(['retry', 'Retry', () => act('resume', d.id)]);
  if (d.path) acts.push(['reveal', 'Reveal in Finder', () => act('reveal_download', d.id)]);
  acts.push(['remove', 'Remove', () => { state.selected = new Set([d.id]); render(); openRemove(); }]);
  return acts;
}

function updateRow(li, d) {
  const name = nameOf(d);
  li.className = `row ${d.status}${state.selected.has(d.id) ? ' selected' : ''}`;
  li.querySelector('.glyph').textContent = d.kind === 'hls' ? 'HLS' : d.kind === 'video' ? 'VID' : (extOf(name).slice(0, 4) || 'FILE');
  const nameEl = li.querySelector('.name');
  nameEl.textContent = name;
  nameEl.title = name;

  const size = d.total ?? (d.status === 'completed' || d.status === 'running' ? d.downloaded : null);
  const sizeText = d.status === 'running' && d.total
    ? `${fmtBytes(d.downloaded)} / ${fmtBytes(d.total)}`
    : size != null ? fmtBytes(size) : '—';
  li.querySelector('.meta').textContent = [sizeText, hostOf(d.url), statusText(d)].filter(Boolean).join('  ·  ');
  li.querySelector('.meta').title = d.error || d.url;

  li.querySelector('.speed').textContent = d.status === 'running' ? fmtSpeed(d.speed) : '';
  const f = fraction(d);
  li.querySelector('.pct').textContent = d.status === 'completed' ? '' : f != null ? `${Math.floor(f * 100)}%` : '';
  const st = li.querySelector('.state');
  st.className = `state ${d.status}`;
  st.textContent = d.status === 'queued' && d.retry_at > Date.now() ? 'Retry'
    : d.status === 'queued' && scheduleBlocks(d) ? 'Later' : STATE_LABEL[d.status] || d.status;

  const bar = li.querySelector('.bar');
  bar.classList.toggle('indeterminate', f == null && d.status === 'running');
  bar.firstElementChild.style.width = `${((f ?? 0) * 100).toFixed(2)}%`;

  // Rebuild hover actions only when the set changes.
  const acts = rowActions(d);
  const sig = acts.map((a) => a[0]).join();
  const box = li.querySelector('.actions');
  if (box.dataset.sig !== sig) {
    box.dataset.sig = sig;
    box.replaceChildren(...acts.map(([icon, title, fn]) => {
      const b = document.createElement('button');
      b.innerHTML = ICONS[icon];
      b.title = title;
      b.onmousedown = (e) => e.stopPropagation();
      b.onclick = (e) => { e.stopPropagation(); fn(); };
      return b;
    }));
  }
}

function render() {
  const tbody = $('#rows');
  const all = [...state.items.values()].sort((a, b) => b.id - a.id);
  const visible = all.filter(matches);
  state.visible = visible.map((d) => d.id);

  // Drop selection of rows that left the view.
  for (const id of [...state.selected]) if (!state.visible.includes(id)) state.selected.delete(id);

  const keep = new Set(state.visible);
  for (const [id, tr] of state.rows) {
    if (!keep.has(id)) { tr.remove(); state.rows.delete(id); }
  }
  let prev = null;
  for (const d of visible) {
    let tr = state.rows.get(d.id);
    if (!tr) { tr = makeRow(d.id); state.rows.set(d.id, tr); }
    updateRow(tr, d);
    const want = prev ? prev.nextSibling : tbody.firstChild;
    if (want !== tr) tbody.insertBefore(tr, want);
    prev = tr;
  }
  $('#empty').classList.toggle('hidden', visible.length > 0);
  $('main').classList.toggle('show-stats', state.view === 'stats');
  $('#stats').classList.toggle('hidden', state.view !== 'stats');
  const filterLabel = state.category || STATUS_FILTERS.find(([k]) => k === state.filter)?.[1] || 'All';
  $('#list-title').textContent = filterLabel;
  $('#list-count').textContent = visible.length ? String(visible.length).padStart(2, '0') : '';
  renderSidebar(all);
  renderToolbar();
  renderDetail();
  renderStatusbar(all);
}

function renderSidebar(all) {
  $('#nav-status').replaceChildren(...STATUS_FILTERS.map(([key, label, pred], i) => {
    const li = navItem(String(i + 1), label, all.filter(pred).length, state.filter === key && !state.category);
    li.onclick = () => selectFilter(key);
    return li;
  }));
  const allCats = [...(state.settings?.categories || []).map((c) => c.name), 'Other'];
  // Collapsed, only the type you're filtering by (if any) stays visible.
  const active = state.view === 'list' && state.category;
  const cats = state.typesOpen ? allCats : allCats.filter((c) => c === active);
  const body = $('#types-body');
  body.classList.toggle('closed', !state.typesOpen);
  body.classList.toggle('peek', !state.typesOpen && cats.length > 0);
  $('#types-toggle').setAttribute('aria-expanded', String(state.typesOpen));
  $('#nav-category').replaceChildren(...cats.map((name, i) => {
    const count = all.filter((d) => (d.category || 'Other') === name).length;
    const li = navItem(i === cats.length - 1 ? '└' : '├', name, count, active === name);
    li.onclick = () => { state.view = 'list'; state.category = name; state.filter = 'all'; render(); };
    return li;
  }));
  const stats = navItem('6', 'Stats', 0, state.view === 'stats');
  stats.onclick = openStats;
  $('#nav-insights').replaceChildren(stats);
  // Library items only look selected while the list is showing.
  if (state.view === 'stats') for (const li of $$('#nav-status li')) li.classList.remove('active');
}

function selectFilter(key) {
  state.view = 'list';
  state.filter = key;
  state.category = null;
  render();
}

function navItem(key, label, count, active) {
  const li = document.createElement('li');
  const k = document.createElement('span');
  k.className = 'key';
  k.textContent = key;
  const a = document.createElement('span');
  a.className = 'name';
  a.textContent = label;
  const c = document.createElement('span');
  c.className = 'count';
  c.textContent = count ? String(count).padStart(2, '0') : '';
  li.append(k, a, c);
  li.classList.toggle('active', active);
  return li;
}

function selectedItems() {
  return [...state.selected].map((id) => state.items.get(id)).filter(Boolean);
}

function renderToolbar() {
  const sel = selectedItems();
  $('#btn-resume').disabled = !sel.some((d) => d.status === 'paused' || d.status === 'failed');
  $('#btn-pause').disabled = !sel.some((d) => d.status === 'running' || d.status === 'queued');
  $('#btn-delete').disabled = sel.length === 0;
  const [num, unit] = speedParts(state.totalSpeed);
  $('#total-speed').textContent = num;
  $('#total-unit').textContent = unit;
}

function drawSpark() {
  const canvas = $('#spark');
  const w = canvas.clientWidth, h = canvas.clientHeight;
  if (!w) return;
  const dpr = window.devicePixelRatio || 1;
  canvas.width = w * dpr;
  canvas.height = h * dpr;
  const ctx = canvas.getContext('2d');
  ctx.scale(dpr, dpr);
  const css = getComputedStyle(document.documentElement);
  const data = state.speeds;
  const max = Math.max(1, ...data);
  const step = w / Math.max(1, SPARK_POINTS - 1);
  const x0 = w - (data.length - 1) * step;
  // Baseline of faint ticks, like a terminal meter.
  ctx.fillStyle = css.getPropertyValue('--cell-left');
  for (let x = 0; x < w; x += 4) ctx.fillRect(x, h - 1, 2, 1);
  if (data.length < 2) return;
  ctx.beginPath();
  data.forEach((v, i) => {
    const x = x0 + i * step;
    const y = h - 2 - (v / max) * (h - 6);
    i ? ctx.lineTo(x, y) : ctx.moveTo(x, y);
  });
  ctx.strokeStyle = css.getPropertyValue('--text');
  ctx.lineWidth = 1.25;
  ctx.stroke();
  ctx.lineTo(w, h);
  ctx.lineTo(x0, h);
  ctx.closePath();
  ctx.fillStyle = css.getPropertyValue('--selected');
  ctx.fill();
}

const SPARK_POINTS = 120; // a minute at two samples per second

function renderStatusbar(all) {
  const running = all.filter((d) => d.status === 'running').length;
  const queued = all.filter((d) => d.status === 'queued').length;
  const parts = [];
  if (running) parts.push(`${running} active`);
  if (queued) parts.push(`${queued} queued`);
  parts.push(`${all.length} total`);
  $('#status-left').textContent = parts.join('  ·  ');
  const s = state.settings;
  const right = [];
  if (s?.speed_limit) right.push(`limit ${fmtSpeed(s.speed_limit)}`);
  if (s?.schedule?.enabled) right.push(`schedule ${s.schedule.start}–${s.schedule.stop}`);
  right.push(state.totalSpeed > 0 ? `↓ ${fmtSpeed(state.totalSpeed)}` : 'idle');
  $('#status-right').textContent = right.join('  ·  ');
}

// ---------- command palette ----------

const palette = { items: [], index: 0 };

function paletteCommands() {
  const s = state.settings || {};
  const cmds = [
    ['ADD', 'Add URL', '⌘N', () => openAdd()],
    ['ADD', 'Download link from clipboard', '', () => openAdd()],
    ['▶', 'Resume all', '', () => invoke('resume_all')],
    ['II', 'Pause all', '', () => invoke('pause_all')],
    ['ALL', 'Show all downloads', '1', () => selectFilter('all')],
    ['ACT', 'Show active', '2', () => selectFilter('active')],
    ['PSE', 'Show paused', '3', () => selectFilter('paused')],
    ['OK', 'Show finished', '4', () => selectFilter('completed')],
    ['ERR', 'Show failed', '5', () => selectFilter('failed')],
    ['STA', 'Open stats', '6', () => openStats()],
    ['CLR', 'Clear finished from the list', '', () => invoke('clear_completed')],
    ['SET', 'Settings', '⌘,', () => openSettings()],
    ['MAX', 'Speed: full', state.profile === 'full' ? '●' : '', () => invoke('set_profile', { profile: 'full' })],
    ['BG', `Speed: background (${fmtSpeed(s.background_limit || 0)})`, state.profile === 'background' ? '●' : '', () => invoke('set_profile', { profile: 'background' })],
    ['II', 'Speed: pause everything', state.profile === 'paused' ? '●' : '', () => invoke('set_profile', { profile: 'paused' })],
    ['CLP', `${s.clipboard_watch ? 'Stop' : 'Start'} watching the clipboard`, '', () => toggleSetting('clipboard_watch')],
    ['ZZZ', `${s.keep_awake ? 'Allow' : 'Prevent'} sleep while downloading`, '', () => toggleSetting('keep_awake')],
    ['RTY', `${s.auto_retry ? 'Turn off' : 'Turn on'} automatic retries`, '', () => toggleSetting('auto_retry')],
  ];
  return cmds.map(([icon, title, hint, run]) => ({ icon, title, hint, run, group: 'Commands' }));
}

function paletteDownloads() {
  return [...state.items.values()].sort((a, b) => b.id - a.id).map((d) => ({
    icon: d.kind === 'hls' ? 'HLS' : d.kind === 'video' ? 'VID' : (extOf(nameOf(d)).slice(0, 3) || 'FIL'),
    title: nameOf(d),
    sub: `${STATE_LABEL[d.status] || d.status} · ${hostOf(d.url)}`,
    hint: '',
    group: 'Downloads',
    run: () => {
      state.view = 'list';
      state.filter = 'all';
      state.category = null;
      state.selected = new Set([d.id]);
      render();
      state.rows.get(d.id)?.scrollIntoView({ block: 'nearest' });
    },
  }));
}

/** Subsequence match: every typed character in order. Lower is better. */
function fuzzy(query, text) {
  if (!query) return 0;
  const t = text.toLowerCase();
  let pos = -1, score = 0;
  for (const ch of query.toLowerCase()) {
    const next = t.indexOf(ch, pos + 1);
    if (next < 0) return null;
    score += next - pos - 1;
    pos = next;
  }
  // Letters scattered across the whole text aren't a real match.
  if (score > 4 * query.length + 4) return null;
  return score + (t.startsWith(query.toLowerCase()) ? -100 : 0);
}

function renderPalette() {
  const q = $('#p-input').value.trim();
  const pick = (items) => items
    .map((it) => ({ it, s: fuzzy(q, `${it.title} ${it.sub || ''}`) }))
    .filter((x) => x.s !== null)
    .sort((a, b) => a.s - b.s)
    .map((x) => x.it);
  const cmds = pick(paletteCommands());
  const dls = pick(paletteDownloads()).slice(0, q ? 30 : 8);
  palette.items = [...cmds, ...dls];
  palette.index = Math.min(palette.index, Math.max(0, palette.items.length - 1));

  const list = $('#p-list');
  const rows = [];
  let group = '';
  palette.items.forEach((it, i) => {
    if (it.group !== group) {
      group = it.group;
      rows.push(el('li', 'group', group));
    }
    const li = el('li');
    li.setAttribute('role', 'option');
    li.classList.toggle('on', i === palette.index);
    const t = el('span', 't', it.title);
    if (it.sub) t.append(el('small', null, it.sub));
    li.append(el('span', 'icon', it.icon), t, el('span', 'hint', it.hint || ''));
    li.onmousemove = () => { if (palette.index !== i) { palette.index = i; renderPalette(); } };
    li.onclick = () => runPalette(i);
    rows.push(li);
  });
  if (!palette.items.length) rows.push(el('li', 'empty-row', 'No matches'));
  list.replaceChildren(...rows);
  list.querySelector('li.on')?.scrollIntoView({ block: 'nearest' });
}

function openPalette() {
  const dlg = $('#palette');
  if (dlg.open) { dlg.close(); return; }
  hideMenu();
  $('#p-input').value = '';
  palette.index = 0;
  renderPalette();
  dlg.showModal();
  $('#p-input').focus();
}

async function runPalette(i) {
  const it = palette.items[i];
  if (!it) return;
  $('#palette').close();
  try { await it.run(); } catch (e) { toast(String(e)); }
}

async function toggleSetting(key) {
  const settings = { ...state.settings, [key]: !state.settings[key] };
  state.settings = await invoke('save_settings', { settings });
  render();
}

// ---------- stats ----------

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
}

/** A big number with its unit set smaller: "12.4" + "GB". */
function bigValue(num, unit) {
  const v = el('div', 'v', num);
  if (unit) v.append(el('small', null, unit));
  return v;
}

function bytesValue(n) {
  const [num, unit] = fmtBytes(n || 0).split(' ');
  return bigValue(num, unit);
}

function speedValue(n) {
  const [num, unit] = speedParts(n);
  return bigValue(num, unit);
}

function tile(key, value, sub) {
  const t = el('div', 'tile');
  t.append(el('div', 'k', key), value);
  if (sub) t.append(el('div', 'sub', sub));
  return t;
}

function panel(title, aside, body, cls = '') {
  const p = el('div', `panel ${cls}`);
  const head = el('div', 'panel-head');
  head.append(el('h2', null, title), el('span', 'aside', aside || ''));
  p.append(head, ...[].concat(body));
  return p;
}

function dayLabel(iso, opts = { weekday: 'short', day: 'numeric', month: 'short' }) {
  return new Date(`${iso}T12:00:00`).toLocaleDateString([], opts);
}

/** Columns on a shared baseline; zero values show as a faint stub. */
function columns(values, tipFor, cls = '') {
  const max = Math.max(1, ...values);
  const box = el('div', `cols ${cls}`);
  values.forEach((v, i) => {
    const c = el('div', v ? 'c' : 'c zero');
    const bar = el('i');
    bar.style.height = v ? `${Math.max(3, (v / max) * 100)}%` : '2px';
    c.append(bar);
    c.dataset.tip = tipFor(i, v);
    box.append(c);
  });
  return box;
}

function axis(labels) {
  const a = el('div', 'axis');
  for (const l of labels) a.append(el('span', null, l));
  return a;
}

function heatmap(days) {
  const max = Math.max(1, ...days.map((d) => d.bytes));
  const grid = el('div', 'heat');
  // Start the first column on Monday so rows line up with weekdays.
  const lead = (new Date(`${days[0].date}T12:00:00`).getDay() + 6) % 7;
  for (let i = 0; i < lead; i++) grid.append(el('i', 'future'));
  for (const d of days) {
    const cell = el('i');
    cell.dataset.l = d.bytes ? String(Math.min(4, Math.ceil((d.bytes / max) * 4))) : '0';
    cell.dataset.tip = `${dayLabel(d.date)}\n${d.bytes ? fmtBytes(d.bytes) : 'nothing'}`;
    grid.append(cell);
  }
  const legend = el('div', 'legend');
  legend.append(el('span', null, 'less'));
  for (const l of ['0', '1', '2', '3', '4']) {
    const sw = el('i');
    sw.style.background = l === '0' ? 'var(--cell-left)' : `color-mix(in srgb, var(--text) ${Number(l) * 25}%, transparent)`;
    legend.append(sw);
  }
  legend.append(el('span', null, 'more'));
  return [grid, legend];
}

function ranks(groups, empty) {
  const list = el('ul', 'ranks');
  if (!groups.length) {
    list.append(el('li', 'none', empty));
    return list;
  }
  const max = groups[0].bytes || 1;
  for (const g of groups) {
    const li = el('li');
    li.append(el('span', 'n', g.name), el('span', 'b', `${fmtBytes(g.bytes)} · ${g.count}`));
    const track = el('span', 't');
    const fill = el('i');
    fill.style.width = `${(g.bytes / max) * 100}%`;
    track.append(fill);
    li.append(track);
    li.dataset.tip = `${g.name}\n${fmtBytes(g.bytes)} in ${g.count} file${g.count === 1 ? '' : 's'}`;
    list.append(li);
  }
  return list;
}

function renderStats(s) {
  const root = $('#stats');
  const head = el('div', 'stats-head');
  head.append(el('h1', null, 'Stats'), el('span', 'since', s.since ? `counting since ${dayLabel(s.since, { day: 'numeric', month: 'short', year: 'numeric' })}` : 'counting from now'));

  const big = el('div', 'tiles');
  big.append(
    tile('Total downloaded', bytesValue(s.lifetime_bytes)),
    tile('Today', bytesValue(s.today_bytes)),
    tile('Last 7 days', bytesValue(s.week_bytes)),
    tile('Last 30 days', bytesValue(s.month_bytes)),
  );
  const small = el('div', 'tiles small');
  small.append(
    tile('Files done', bigValue(String(s.completed)), s.largest ? `largest · ${s.largest.name}` : null),
    tile('Failed', bigValue(String(s.failed))),
    tile('Average speed', speedValue(s.avg_speed), 'while downloading'),
    tile('Peak speed', speedValue(s.peak_speed), 'single download'),
  );

  const month = s.days.slice(-30);
  const monthPanel = panel('Last 30 days', fmtBytes(s.month_bytes), [
    columns(month.map((d) => d.bytes), (i, v) => `${dayLabel(month[i].date)}\n${v ? fmtBytes(v) : 'nothing'}`),
    axis([dayLabel(month[0].date, { day: 'numeric', month: 'short' }), dayLabel(month[15].date, { day: 'numeric', month: 'short' }), 'today']),
  ]);
  const heatPanel = panel('Activity', '12 weeks', heatmap(s.days));

  const hourPanel = panel('Time of day', 'all time', [
    columns(s.hours, (h, v) => `${String(h).padStart(2, '0')}:00–${String(h + 1).padStart(2, '0')}:00\n${v ? fmtBytes(v) : 'nothing'}`, 'short'),
    axis(['00', '06', '12', '18', '23']),
  ], 'full');

  const row1 = el('div', 'panels');
  row1.append(monthPanel, heatPanel);
  const row2 = el('div', 'panels even');
  row2.style.gridTemplateColumns = '1fr 1fr';
  row2.append(
    panel('By type', `${s.categories.length} types`, ranks(s.categories, 'nothing finished yet')),
    panel('Top sites', `top ${s.hosts.length}`, ranks(s.hosts, 'nothing finished yet')),
    hourPanel,
  );
  root.replaceChildren(head, big, small, row1, row2);
}

async function loadStats() {
  try {
    renderStats(await invoke('get_stats'));
  } catch (e) {
    toast(String(e));
  }
}

function openStats() {
  state.view = 'stats';
  state.selected.clear();
  render();
  loadStats();
}

function bindTooltips() {
  const tip = $('#tip');
  const root = $('#stats');
  root.addEventListener('mousemove', (e) => {
    const target = e.target.closest('[data-tip]');
    if (!target) { tip.classList.add('hidden'); return; }
    tip.textContent = target.dataset.tip;
    tip.classList.remove('hidden');
    const r = tip.getBoundingClientRect();
    const x = Math.min(e.clientX + 14, innerWidth - r.width - 8);
    const y = e.clientY - r.height - 12 < 8 ? e.clientY + 16 : e.clientY - r.height - 12;
    tip.style.left = `${x}px`;
    tip.style.top = `${y}px`;
  });
  root.addEventListener('mouseleave', () => tip.classList.add('hidden'));
}

// ---------- detail ----------

function renderDetail() {
  const sel = selectedItems();
  const panel = $('#detail');
  if (state.view === 'stats') { panel.classList.add('hidden'); return; }
  if (sel.length !== 1) { panel.classList.add('hidden'); return; }
  const d = sel[0];
  panel.classList.remove('hidden');
  $('#d-name').textContent = nameOf(d);
  $('#d-url').textContent = d.url;
  $('#d-url').title = d.url;
  const where = d.path || d.dir;
  $('#d-path').textContent = where;
  $('#d-path').title = where;
  $('#d-status').textContent = d.error ? `failed · ${d.error}` : statusText(d);
  $('#d-status').title = d.error || '';
  const f = fraction(d);
  $('#d-progress').textContent = d.parts
    ? `${fmtBytes(d.downloaded)} · ${d.parts[0]} of ${d.parts[1]} segments`
    : `${fmtBytes(d.downloaded)}${d.total ? ` of ${fmtBytes(d.total)}` : ''}${f != null ? ` (${(f * 100).toFixed(1)}%)` : ''}`;
  $('#d-conns').textContent = d.status === 'running'
    ? `${d.active_connections} / ${d.connections} live`
    : `${d.connections}${d.resumable ? '' : ' · no resume'}`;
  $('#d-checksum').textContent = d.checksum || '—';
  if (document.activeElement !== $('#d-limit')) $('#d-limit').value = fmtRate(d.speed_limit);
  $('#d-open').disabled = d.status !== 'completed';
  $('#d-reveal').disabled = !d.path;
  $('#d-relink').classList.toggle('hidden', !canRelink(d));
  drawMap(d);
}

/**
 * The segment map: a strip of cells, each a slice of the file. Filled = on
 * disk, faint = missing, mid = a connection is working there right now.
 */
function drawMap(d) {
  const canvas = $('#d-map');
  const css = getComputedStyle(document.documentElement);
  const dpr = window.devicePixelRatio || 1;
  const w = canvas.clientWidth, h = canvas.clientHeight;
  if (!w) return;
  canvas.width = w * dpr;
  canvas.height = h * dpr;
  const ctx = canvas.getContext('2d');
  ctx.scale(dpr, dpr);

  const size = 5, gap = 2, rows = 3;
  const cols = Math.floor((w + gap) / (size + gap));
  const cells = cols * rows;
  const colors = {
    done: css.getPropertyValue('--cell-done'),
    left: css.getPropertyValue('--cell-left'),
    active: css.getPropertyValue('--cell-active'),
  };

  const missing = state.segments.get(d.id);
  const f = fraction(d) ?? 0;
  const live = d.status === 'running' && d.total && missing;
  for (let i = 0; i < cells; i++) {
    // Fill column by column so the map reads left to right like the file.
    const col = Math.floor(i / rows), row = i % rows;
    const idx = col * rows + row;
    let kind = 'done';
    if (live) {
      const start = (idx / cells) * d.total, end = ((idx + 1) / cells) * d.total;
      const hole = missing.find((s) => s.start < end && s.end > start);
      if (hole) kind = hole.active && hole.start >= start && hole.start < end ? 'active' : 'left';
    } else if (idx / cells >= f) {
      kind = 'left';
    }
    ctx.fillStyle = colors[kind];
    ctx.fillRect(col * (size + gap), row * (size + gap) + 1, size, size);
  }
}

async function refreshSegments() {
  const sel = selectedItems();
  if (sel.length === 1 && sel[0].status === 'running' && sel[0].kind === 'file') {
    try {
      state.segments.set(sel[0].id, await invoke('segments', { id: sel[0].id }));
      drawMap(sel[0]);
    } catch { /* download just finished */ }
  }
}

// ---------- selection & actions ----------

function onRowClick(e, id) {
  if (e.button !== 0) return;
  if (e.metaKey) {
    state.selected.has(id) ? state.selected.delete(id) : state.selected.add(id);
    state.anchor = id;
  } else if (e.shiftKey && state.anchor != null) {
    const a = state.visible.indexOf(state.anchor), b = state.visible.indexOf(id);
    state.selected = new Set(state.visible.slice(Math.min(a, b), Math.max(a, b) + 1));
  } else {
    state.selected = new Set([id]);
    state.anchor = id;
  }
  render();
}

function onRowDouble(id) {
  const d = state.items.get(id);
  if (!d) return;
  if (d.status === 'completed') act('open_download', id);
  else if (d.status === 'running' || d.status === 'queued') act('pause', id);
  else act('resume', id);
}

async function act(cmd, id, extra = {}) {
  try { await invoke(cmd, { id, ...extra }); } catch (e) { toast(String(e)); }
}

async function forSelected(cmd, pred) {
  for (const d of selectedItems().filter(pred)) await act(cmd, d.id);
}

function toast(msg) {
  $('#status-left').textContent = msg;
}

function onRowMenu(e, id) {
  e.preventDefault();
  if (!state.selected.has(id)) { state.selected = new Set([id]); state.anchor = id; render(); }
  const sel = selectedItems();
  const one = sel.length === 1 ? sel[0] : null;
  const items = [];
  if (one?.status === 'completed') items.push(['Open', () => act('open_download', one.id)]);
  if (one?.path) items.push(['Show in Finder', () => act('reveal_download', one.id)]);
  if (items.length) items.push('-');
  if (sel.some((d) => d.status === 'paused' || d.status === 'failed')) {
    items.push(['Resume', () => forSelected('resume', (d) => d.status === 'paused' || d.status === 'failed')]);
  }
  if (sel.some((d) => d.status === 'running' || d.status === 'queued')) {
    items.push(['Pause', () => forSelected('pause', (d) => d.status === 'running' || d.status === 'queued')]);
  }
  if (sel.some((d) => d.status === 'completed' || d.status === 'failed')) {
    items.push(['Download again', () => forSelected('redownload', (d) => d.status === 'completed' || d.status === 'failed')]);
  }
  if (one) items.push(['Copy URL', () => invoke('copy_text', { text: one.url })]);
  if (one && canRelink(one)) items.push(['Refresh link…', () => openRelink(one)]);
  items.push('-');
  items.push(['Remove…', () => openRemove(), 'danger-item']);
  showMenu(e.clientX, e.clientY, items);
}

function showMenu(x, y, items) {
  const menu = $('#menu');
  menu.replaceChildren(...items.map((it) => {
    if (it === '-') return document.createElement('hr');
    const b = document.createElement('button');
    b.textContent = it[0];
    if (it[2]) b.className = it[2];
    b.onclick = () => { hideMenu(); it[1](); };
    return b;
  }));
  menu.classList.remove('hidden');
  const r = menu.getBoundingClientRect();
  menu.style.left = `${Math.min(x, innerWidth - r.width - 8)}px`;
  menu.style.top = `${Math.min(y, innerHeight - r.height - 8)}px`;
}

function hideMenu() { $('#menu').classList.add('hidden'); }

// ---------- add dialog ----------

/** How many URLs the text expands to (mirrors the Rust pattern rules). */
function countUrls(text) {
  let total = 0;
  for (const line of text.split('\n').map((l) => l.trim()).filter(Boolean)) {
    let n = 1;
    for (const m of line.matchAll(/\[([^\]]*)\]/g)) {
      const r = m[1].match(/^(\d+)-(\d+)$/) || m[1].match(/^([a-zA-Z])-([a-zA-Z])$/);
      if (!r) continue;
      const a = /\d/.test(r[1]) ? Number(r[1]) : r[1].charCodeAt(0);
      const b = /\d/.test(r[2]) ? Number(r[2]) : r[2].charCodeAt(0);
      if (b >= a) n *= b - a + 1;
    }
    total += n;
  }
  return total;
}

const VIDEO_SITES = ['youtube.com', 'youtu.be', 'vimeo.com', 'x.com', 'twitter.com', 'tiktok.com', 'instagram.com',
  'facebook.com', 'fb.watch', 'reddit.com', 'twitch.tv', 'dailymotion.com', 'soundcloud.com', 'bilibili.com',
  'streamable.com', 'loom.com', 'bandcamp.com', 'ted.com', 'nicovideo.jp'];

function isVideoSite(url) {
  const host = hostOf(url).replace(/^(www|m)\./, '');
  return VIDEO_SITES.some((s) => host === s || host.endsWith(`.${s}`));
}

function updateAddCount() {
  const text = $('#a-urls').value;
  const n = countUrls(text);
  $('#a-count').textContent = n > 1 ? `${n} files will be added` : '';
  $('#a-name').disabled = n > 1;
  // Quality only matters for video sites.
  const kind = $('#a-kind').value;
  const firstUrl = text.split('\n').map((l) => l.trim()).find(Boolean) || '';
  const video = kind === 'video' || (kind === '' && isVideoSite(firstUrl));
  $('#a-quality-field').style.visibility = video ? 'visible' : 'hidden';
  checkDuplicates();
}

let dupeTimer = 0;

/** Warns when the link (or a file of that name and size) is already in the list. */
function checkDuplicates() {
  clearTimeout(dupeTimer);
  dupeTimer = setTimeout(async () => {
    const urls = $('#a-urls').value.split('\n').map((u) => u.trim()).filter(Boolean);
    let found = [];
    if (urls.length === 1 && /^https?:\/\//i.test(urls[0])) {
      try {
        found = await invoke('find_duplicates', {
          url: urls[0],
          file_name: $('#a-name').value.trim() || null,
          size: state.pending?.size ?? null,
        });
      } catch { /* not worth an error */ }
    }
    showDuplicate($('#a-dupe'), found[0]);
    $('#a-submit').firstChild.textContent = found.length ? 'Download again ' : 'Download ';
  }, 200);
}

function showDuplicate(note, d) {
  note.classList.toggle('hidden', !d);
  if (!d) return;
  const done = d.status === 'completed';
  const where = d.path ? d.path.replace(/\/[^/]+$/, '') : '';
  note.replaceChildren(
    document.createTextNode(done ? 'Already downloaded: ' : `Already in your list (${STATE_LABEL[d.status] || d.status}): `),
    el('b', '', d.name),
    el('span', 'muted', [d.total ? fmtBytes(d.total) : '', where && `in ${tildify(where)}`].filter(Boolean).map((t) => `  ·  ${t}`).join('')),
  );
}

async function openAdd(prefill) {
  const s = state.settings;
  state.pending = prefill || null;
  $('#form-add').reset();
  $('#a-error').classList.add('hidden');
  await fillAddFolders(loadPref('lastDir', ''));
  $('#a-conns').value = s?.connections ?? 8;
  $('#a-start').checked = true;
  const note = prefill?.source === 'clipboard' ? 'Copied to your clipboard just now.'
    : prefill?.headers?.length ? 'From your browser · cookies included, so files behind a login work.' : '';
  $('#a-browser').textContent = note;
  $('#a-browser').classList.toggle('hidden', !note);
  $('#a-kind').value = prefill?.kind || '';
  $('#a-quality').value = prefill?.quality || s?.video_quality || 'best';
  if (prefill) {
    $('#a-urls').value = (prefill.urls || []).join('\n');
    $('#a-name').value = prefill.file_name || '';
  } else {
    try {
      const clip = (await invoke('clipboard_text')).trim();
      if (/^https?:\/\/\S+$/i.test(clip) && !clip.includes('\n')) $('#a-urls').value = clip;
    } catch { /* clipboard unavailable */ }
  }
  updateAddCount();
  $('#dlg-add').showModal();
  $('#a-urls').focus();
  $('#a-urls').select();
}

async function submitAdd(e) {
  e.preventDefault();
  const err = $('#a-error');
  const limit = parseRate($('#a-limit').value);
  if (isNaN(limit)) { showErr(err, 'Speed limit should look like 500K or 2M.'); return; }
  const req = {
    urls: $('#a-urls').value.split('\n').map((u) => u.trim()).filter(Boolean),
    dir: $('#a-dir').value || null,
    file_name: $('#a-name').disabled ? null : ($('#a-name').value.trim() || null),
    connections: Number($('#a-conns').value) || null,
    speed_limit: limit,
    checksum: $('#a-checksum').value.trim() || null,
    headers: state.pending?.headers || [],
    kind: $('#a-kind').value || null,
    quality: $('#a-quality-field').style.visibility === 'hidden' ? null : $('#a-quality').value,
    start: $('#a-start').checked,
  };
  if (!req.urls.length) { showErr(err, 'Paste at least one URL.'); return; }
  try {
    const ids = await invoke('add_downloads', { req });
    rememberFolder(req.dir);
    $('#dlg-add').close();
    state.pending = null;
    state.filter = 'all';
    state.category = null;
    await reload();
    state.selected = new Set(ids.slice(0, 1));
    render();
  } catch (x) {
    showErr(err, String(x));
  }
}

function showErr(el, msg) {
  el.textContent = msg;
  el.classList.remove('hidden');
}

// ---------- folder dropdown (add dialog) ----------

let homeDir = null;

async function home() {
  if (homeDir === null) {
    try { homeDir = await window.__TAURI__.path.homeDir(); } catch { homeDir = ''; }
    homeDir ||= (state.settings?.download_dir || '').match(/^\/Users\/[^/]+/)?.[0] || '';
  }
  return homeDir;
}

function tildify(path) {
  return homeDir && path.startsWith(homeDir) ? `~${path.slice(homeDir.length)}` : path;
}

function folderName(path) {
  return path.split('/').filter(Boolean).pop() || path;
}

/** Fills Save to with RDM's default, recent folders and common ones; '' is the default. */
async function fillAddFolders(selected) {
  const h = await home();
  const def = state.settings?.download_dir || '';
  const seen = new Set();
  const options = [];
  const add = (value, label) => {
    if (seen.has(value)) return;
    seen.add(value);
    options.push([value, label]);
  };
  add('', def ? `${folderName(def)}  —  default` : 'Default folder');
  for (const p of loadPref('recentDirs', [])) add(p, `${folderName(p)}  —  ${tildify(p)}`);
  if (h) for (const d of ['Desktop', 'Documents', 'Movies', 'Music']) if (`${h}/${d}` !== def) add(`${h}/${d}`, `${d}  —  ~/${d}`);
  if (selected && !seen.has(selected)) add(selected, `${folderName(selected)}  —  ${tildify(selected)}`);
  $('#a-dir').replaceChildren(...options.map(([value, label]) => {
    const o = document.createElement('option');
    o.value = value;
    o.textContent = label;
    return o;
  }));
  $('#a-dir').value = seen.has(selected) ? selected : '';
  $('#a-dir').title = $('#a-dir').value || def;
}

/** Remembers the folder for next time; the last five picked show in the list. */
function rememberFolder(dir) {
  savePref('lastDir', dir || '');
  if (dir) savePref('recentDirs', [dir, ...loadPref('recentDirs', []).filter((p) => p !== dir)].slice(0, 5));
}

async function chooseAddFolder() {
  try {
    const dir = await invoke('choose_folder', { start: $('#a-dir').value || state.settings?.download_dir || null });
    if (dir) await fillAddFolders(dir);
  } catch (e) { toast(String(e)); }
}

async function chooseFolder(input) {
  try {
    const dir = await invoke('choose_folder', { start: input.value.trim() || input.placeholder || null });
    if (dir) input.value = dir;
  } catch (e) { toast(String(e)); }
}

// ---------- refresh link ----------

let relinkId = null;

/** Paused or failed file downloads can carry on from a new link. */
function canRelink(d) {
  return d.kind === 'file' && ['paused', 'failed', 'queued'].includes(d.status);
}

async function openRelink(d) {
  relinkId = d.id;
  $('#form-relink').reset();
  $('#l-error').classList.add('hidden');
  $('#l-name').textContent = nameOf(d);
  $('#l-name').title = d.url;
  $('#l-size').textContent = d.total ? fmtBytes(d.total) : '';
  $('#l-done').textContent = d.total ? `${fmtBytes(d.downloaded)} of ${fmtBytes(d.total)}` : fmtBytes(d.downloaded);
  // Until the dialog closes, the next browser download is the new link.
  await invoke('await_link', { id: d.id });
  $('#dlg-relink').showModal();
  $('#l-url').focus();
}

async function submitRelink(e) {
  e.preventDefault();
  const err = $('#l-error');
  const url = $('#l-url').value.trim();
  if (!url) { showErr(err, 'Paste the new link, or start the download again in your browser.'); return; }
  $('#l-submit').disabled = true;
  err.classList.add('hidden');
  try {
    await invoke('relink', { id: relinkId, url });
    $('#dlg-relink').close();
    toast('Link refreshed. Carrying on where it stopped.');
  } catch (x) {
    showErr(err, String(x));
  } finally {
    $('#l-submit').disabled = false;
  }
}

/** The browser sent a link while the dialog was waiting. */
function onRelinked({ id, ok, error }) {
  if (id !== relinkId || !$('#dlg-relink').open) return;
  if (ok) {
    $('#dlg-relink').close();
    toast('Link refreshed from the browser. Carrying on where it stopped.');
  } else {
    showErr($('#l-error'), error);
    invoke('await_link', { id }); // keep waiting for another try
  }
}

// ---------- remove dialog ----------

function openRemove() {
  const sel = selectedItems();
  if (!sel.length) return;
  $('#r-title').textContent = sel.length === 1 ? `Remove “${nameOf(sel[0])}”?` : `Remove ${sel.length} downloads?`;
  const anyDone = sel.some((d) => d.status === 'completed');
  $('#r-files').checked = false;
  $('#r-files').parentElement.classList.toggle('hidden', !anyDone);
  $('#dlg-remove').showModal();
}

async function submitRemove(e) {
  e.preventDefault();
  const deleteFile = $('#r-files').checked;
  for (const d of selectedItems()) await act('remove', d.id, { delete_file: deleteFile });
  state.selected.clear();
  $('#dlg-remove').close();
}

// ---------- settings dialog ----------

function catRow(c = { name: '', folder: '', extensions: [] }) {
  const tr = document.createElement('tr');
  for (const [key, val] of [['name', c.name], ['folder', c.folder], ['extensions', c.extensions.join(' ')]]) {
    const td = document.createElement('td');
    const input = document.createElement('input');
    input.type = 'text';
    input.dataset.key = key;
    input.value = val;
    input.spellcheck = false;
    td.append(input);
    tr.append(td);
  }
  const td = document.createElement('td');
  const del = document.createElement('button');
  del.type = 'button';
  del.className = 'icon';
  del.textContent = '×';
  del.title = 'Remove category';
  del.onclick = () => tr.remove();
  td.append(del);
  tr.append(td);
  return tr;
}

async function openSettings() {
  const s = await invoke('get_settings');
  state.settings = s;
  $('#s-dir').value = s.download_dir;
  $('#s-max').value = s.max_active;
  $('#s-conns').value = s.connections;
  $('#s-limit').value = fmtRate(s.speed_limit);
  $('#s-bglimit').value = fmtRate(s.background_limit);
  $('#s-notify').checked = s.notify;
  $('#s-clipboard').checked = s.clipboard_watch;
  $('#s-awake').checked = s.keep_awake;
  $('#s-retry').checked = s.auto_retry;
  $('#s-quality').value = s.video_quality || 'best';
  try {
    const t = await invoke('tools_status');
    const line = (name, path, hint) => {
      const row = document.createElement('div');
      if (path) {
        const b = document.createElement('b');
        b.textContent = name;
        row.append(b, ` ${path.replace(/^\/Users\/[^/]+/, '~')}`);
      } else {
        row.className = 'missing';
        row.textContent = `${name} missing · ${hint}`;
      }
      return row;
    };
    $('#s-tools').replaceChildren(
      line('yt-dlp', t.ytdlp, 'brew install yt-dlp'),
      line('ffmpeg', t.ffmpeg, 'brew install ffmpeg'),
    );
  } catch { /* ignore */ }
  $('#s-usecat').checked = s.use_categories;
  $('#s-cats').replaceChildren(...s.categories.map(catRow));
  $('#s-sched').checked = s.schedule.enabled;
  $('#s-start').value = s.schedule.start;
  $('#s-stop').value = s.schedule.stop;
  $('#s-confirm').checked = s.confirm_browser_downloads;
  $('#s-port').value = s.bridge_port;
  $('#s-token').value = s.bridge_token;
  $('#s-error').classList.add('hidden');
  try { $('#s-login').checked = await invoke('get_autostart'); } catch { /* ignore */ }
  try {
    const b = await invoke('bridge_status');
    $('#s-bridge-status').textContent = b.error
      ? `Browser bridge is not running: ${b.error}`
      : `Browser bridge is listening on 127.0.0.1:${b.port}.`;
  } catch { /* ignore */ }
  selectTab('general');
  $('#dlg-settings').showModal();
}

function selectTab(name) {
  for (const b of $$('#s-tabs button')) b.classList.toggle('active', b.dataset.tab === name);
  for (const t of $$('#form-settings .tab')) t.classList.toggle('hidden', t.dataset.tab !== name);
}

async function submitSettings(e) {
  e.preventDefault();
  const err = $('#s-error');
  const limit = parseRate($('#s-limit').value);
  if (isNaN(limit)) { showErr(err, 'Speed limit should look like 500K or 2M.'); return; }
  const bgLimit = parseRate($('#s-bglimit').value);
  if (isNaN(bgLimit)) { showErr(err, 'Background speed should look like 500K or 2M.'); return; }
  const categories = [...$('#s-cats').children].map((tr) => {
    const get = (k) => tr.querySelector(`[data-key=${k}]`).value.trim();
    return { name: get('name'), folder: get('folder') || get('name'), extensions: get('extensions').split(/[\s,]+/).filter(Boolean).map((x) => x.replace(/^\./, '').toLowerCase()) };
  }).filter((c) => c.name);
  const oldPort = state.settings.bridge_port;
  const settings = {
    ...state.settings,
    download_dir: $('#s-dir').value.trim() || state.settings.download_dir,
    max_active: Number($('#s-max').value) || 3,
    connections: Number($('#s-conns').value) || 8,
    speed_limit: limit,
    background_limit: bgLimit || 1024 * 1024,
    notify: $('#s-notify').checked,
    clipboard_watch: $('#s-clipboard').checked,
    keep_awake: $('#s-awake').checked,
    auto_retry: $('#s-retry').checked,
    video_quality: $('#s-quality').value,
    use_categories: $('#s-usecat').checked,
    categories,
    schedule: { enabled: $('#s-sched').checked, start: $('#s-start').value || '02:00', stop: $('#s-stop').value || '08:00' },
    confirm_browser_downloads: $('#s-confirm').checked,
    bridge_port: Number($('#s-port').value) || 47321,
    bridge_token: $('#s-token').value,
  };
  try {
    state.settings = await invoke('save_settings', { settings });
    await invoke('set_autostart', { enabled: $('#s-login').checked });
    $('#dlg-settings').close();
    if (settings.bridge_port !== oldPort) toast('Restart RDM to use the new browser port.');
    await reload();
  } catch (x) {
    showErr(err, String(x));
  }
}

function newToken() {
  const bytes = crypto.getRandomValues(new Uint8Array(24));
  $('#s-token').value = [...bytes].map((b) => b.toString(16).padStart(2, '0')).join('');
}

// ---------- events ----------

async function reload() {
  const list = await invoke('list_downloads');
  state.items = new Map(list.map((d) => [d.id, d]));
  render();
}

function onEvent(ev) {
  switch (ev.type) {
    case 'changed':
      state.items.set(ev.download.id, ev.download);
      break;
    case 'removed':
      state.items.delete(ev.id);
      state.selected.delete(ev.id);
      break;
    case 'progress': {
      state.totalSpeed = ev.total_speed;
      state.speeds.push(ev.total_speed);
      if (state.speeds.length > SPARK_POINTS) state.speeds.shift();
      drawSpark();
      for (const s of ev.items) {
        const d = state.items.get(s.id);
        if (!d) continue;
        d.downloaded = s.downloaded;
        d.total = s.total ?? d.total;
        d.speed = s.speed;
        d.active_connections = s.active_connections;
        d.parts = s.parts;
      }
      for (const d of state.items.values()) if (d.status !== 'running') d.speed = 0;
      break;
    }
    default:
      return;
  }
  render();
}

function bind() {
  $('#btn-add').onclick = () => openAdd();
  $('#btn-resume').onclick = () => forSelected('resume', (d) => d.status === 'paused' || d.status === 'failed');
  $('#btn-pause').onclick = () => forSelected('pause', (d) => d.status === 'running' || d.status === 'queued');
  $('#btn-delete').onclick = openRemove;
  $('#btn-resume-all').onclick = () => invoke('resume_all').catch((e) => toast(String(e)));
  $('#btn-pause-all').onclick = () => invoke('pause_all').catch((e) => toast(String(e)));
  $('#btn-settings').onclick = openSettings;
  $('#btn-palette').onclick = openPalette;
  $('#types-toggle').onclick = () => {
    state.typesOpen = !state.typesOpen;
    savePref('typesOpen', state.typesOpen);
    render();
  };
  $('#p-input').oninput = () => { palette.index = 0; renderPalette(); };
  $('#p-input').onkeydown = (e) => {
    if (e.key === 'ArrowDown') { e.preventDefault(); palette.index = Math.min(palette.index + 1, palette.items.length - 1); renderPalette(); }
    else if (e.key === 'ArrowUp') { e.preventDefault(); palette.index = Math.max(palette.index - 1, 0); renderPalette(); }
    else if (e.key === 'Enter') { e.preventDefault(); runPalette(palette.index); }
  };
  $('#search').oninput = (e) => { state.search = e.target.value.trim(); render(); };

  $('#a-urls').oninput = updateAddCount;
  $('#a-kind').onchange = updateAddCount;
  $('#form-add').onsubmit = submitAdd;
  $('#a-cancel').onclick = () => $('#dlg-add').close();
  $('#a-choose').onclick = chooseAddFolder;
  $('#a-dir').onchange = () => { $('#a-dir').title = $('#a-dir').value || state.settings?.download_dir || ''; };
  $('#a-name').oninput = checkDuplicates;
  $('#form-relink').onsubmit = submitRelink;
  $('#l-cancel').onclick = () => $('#dlg-relink').close();
  $('#dlg-relink').addEventListener('close', () => { invoke('await_link', { id: null }); relinkId = null; });
  $('#d-relink').onclick = () => { const [d] = selectedItems(); if (d) openRelink(d); };

  $('#dlg-remove form').onsubmit = submitRemove;
  $('#r-cancel').onclick = () => $('#dlg-remove').close();

  $('#form-settings').onsubmit = submitSettings;
  $('#s-cancel').onclick = () => $('#dlg-settings').close();
  $('#s-choose').onclick = () => chooseFolder($('#s-dir'));
  $('#s-addcat').onclick = () => $('#s-cats').append(catRow());
  $('#s-copy-token').onclick = () => invoke('copy_text', { text: $('#s-token').value });
  $('#s-new-token').onclick = newToken;
  for (const b of $$('#s-tabs button')) b.onclick = () => selectTab(b.dataset.tab);

  $('#d-open').onclick = () => { const [d] = selectedItems(); if (d) act('open_download', d.id); };
  $('#d-reveal').onclick = () => { const [d] = selectedItems(); if (d) act('reveal_download', d.id); };
  $('#d-copy').onclick = () => { const [d] = selectedItems(); if (d) invoke('copy_text', { text: d.url }); };
  $('#d-limit-apply').onclick = () => {
    const [d] = selectedItems();
    const limit = parseRate($('#d-limit').value);
    if (!d) return;
    if (isNaN(limit)) { toast('Speed limit should look like 500K or 2M.'); return; }
    act('set_speed_limit', d.id, { limit });
    $('#d-limit').blur();
  };

  document.addEventListener('mousedown', (e) => { if (!$('#menu').contains(e.target)) hideMenu(); });
  $('.list-wrap').addEventListener('mousedown', (e) => {
    if (e.target.closest('.row') || e.button !== 0) return;
    state.selected.clear();
    render();
  });
  window.addEventListener('resize', () => { renderDetail(); drawSpark(); });
  $('#empty-add').onclick = () => openAdd();

  // Paste a link anywhere (outside text fields) to start a download.
  document.addEventListener('paste', (e) => {
    if (document.querySelector('dialog[open]')) return;
    if (['INPUT', 'TEXTAREA'].includes(document.activeElement?.tagName)) return;
    const text = e.clipboardData?.getData('text') || '';
    const urls = text.split(/\s+/).filter((u) => /^https?:\/\/\S+$/i.test(u));
    if (urls.length) {
      e.preventDefault();
      openAdd({ urls });
    }
  });

  document.addEventListener('keydown', (e) => {
    if (e.metaKey && e.key === 'k') { e.preventDefault(); openPalette(); return; }
    if (document.querySelector('dialog[open]')) return;
    const typing = ['INPUT', 'TEXTAREA'].includes(document.activeElement?.tagName);
    if (e.metaKey && e.key === 'n') { e.preventDefault(); openAdd(); }
    else if (e.metaKey && e.key === ',') { e.preventDefault(); openSettings(); }
    else if (e.metaKey && e.key === 'a' && !typing) { e.preventDefault(); state.selected = new Set(state.visible); render(); }
    else if (typing) {
      if (e.key === 'Escape') document.activeElement.blur();
      return;
    }
    else if (e.key === '/') { e.preventDefault(); $('#search').focus(); }
    else if (/^[1-5]$/.test(e.key) && !e.metaKey) selectFilter(STATUS_FILTERS[Number(e.key) - 1][0]);
    else if (e.key === '6' && !e.metaKey) openStats();
    else if (e.key === 'Backspace' || e.key === 'Delete') openRemove();
    else if (e.key === ' ') {
      e.preventDefault();
      const sel = selectedItems();
      if (sel.some((d) => d.status === 'running' || d.status === 'queued')) $('#btn-pause').click();
      else $('#btn-resume').click();
    } else if (e.key === 'Escape') { state.selected.clear(); render(); }
  });
}

async function main() {
  bind();
  state.settings = await invoke('get_settings');
  await reload();
  await listen('rdm-event', (e) => onEvent(e.payload));
  await listen('rdm-incoming', (e) => openAdd(e.payload));
  await listen('rdm-show-add', () => openAdd());
  await listen('rdm-settings-changed', async () => { state.settings = await invoke('get_settings'); render(); });
  await listen('rdm-relinked', (e) => onRelinked(e.payload));
  await listen('rdm-profile', (e) => { state.profile = e.payload; });
  state.profile = await invoke('get_profile');
  setInterval(refreshSegments, 500);
  drawSpark();
  bindTooltips();
  // Keep the numbers moving while stats are on screen.
  setInterval(() => { if (state.view === 'stats') loadStats(); }, 3000);
  // Relative status text (schedule, ETA) can change without events.
  setInterval(render, 30000);
  setInterval(() => {
    if ([...state.items.values()].some((d) => d.status === 'queued' && d.retry_at > Date.now())) render();
  }, 1000);
}

main().catch((e) => toast(`Could not start: ${e}`));
