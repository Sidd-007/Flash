const $ = (sel) => document.querySelector(sel);
const storage = chrome.storage.session || chrome.storage.local;

function safeName(title) {
  return (title || 'video').replace(/[\\/:*?"<>|]+/g, ' ').replace(/\s+/g, ' ').trim().slice(0, 120) || 'video';
}

function streamLabel(url) {
  try {
    const u = new URL(url);
    return `${u.host} · ${u.pathname.split('/').slice(-2).join('/')}`;
  } catch { return url; }
}

async function main() {
  const s = await getSettings();
  $('#enabled').checked = s.enabled;
  $('#enabled').onchange = (e) => chrome.storage.sync.set({ enabled: e.target.checked });
  $('#options').onclick = () => chrome.runtime.openOptionsPage();
  $('#version').textContent = `v${chrome.runtime.getManifest().version}`;

  pingRdm()
    .then((info) => {
      $('#status').textContent = 'connected';
      $('#status').classList.add('ok');
      $('#version').textContent = `rdm ${info.version}`;
    })
    .catch((e) => {
      $('#status').textContent = 'offline';
      $('#status').classList.add('bad');
      $('#status-detail').textContent = e.message;
      $('#status-detail').hidden = false;
    });

  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  if (!tab) return;
  if (isVideoSite(tab.url || '')) {
    $('#page-video').hidden = false;
    $('#page-video').onclick = () => {
      chrome.runtime.sendMessage({ type: 'rdm-page-video', url: tab.url, title: tab.title?.replace(/ - YouTube$/, '') });
      window.close();
    };
  }
  const key = `streams:${tab.id}`;
  const streams = (await storage.get(key))[key] || [];
  $('#no-streams').hidden = streams.length > 0;

  $('#streams').replaceChildren(...streams.map((stream, i) => {
    const li = document.createElement('li');
    const label = document.createElement('span');
    label.className = 'stream';
    const strong = document.createElement('b');
    strong.textContent = i === 0 ? 'first · ' : '';
    label.append(strong, streamLabel(stream.url));
    label.title = stream.url;
    const btn = document.createElement('button');
    btn.className = 'tiny';
    btn.textContent = 'Get';
    btn.onclick = async () => {
      btn.disabled = true;
      try {
        await sendToRdm(stream.url, { kind: 'hls', referrer: tab.url, file_name: safeName(tab.title) });
        btn.textContent = 'Sent';
      } catch (e) {
        btn.textContent = 'Retry';
        btn.title = e.message;
        btn.disabled = false;
      }
    };
    li.append(label, btn);
    return li;
  }));
}

main();
