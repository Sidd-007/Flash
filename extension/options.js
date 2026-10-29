const $ = (sel) => document.querySelector(sel);

/** Pasted tokens sometimes carry spaces, quotes or line breaks. */
function cleanToken(raw) {
  return raw.replace(/[^0-9a-f]/gi, '').toLowerCase();
}

function checkToken() {
  const token = cleanToken($('#token').value);
  const hint = $('#token-hint');
  hint.hidden = !token || token.length === TOKEN_LENGTH;
  hint.textContent = `The token should be ${TOKEN_LENGTH} characters; this one is ${token.length}. Click Copy in Flash → Settings → Browser and paste again.`;
}

function read() {
  return {
    port: Number($('#port').value) || DEFAULTS.port,
    enabled: $('#enabled').checked,
    captureAll: $('#captureAll').checked,
    askInBrowser: $('#askInBrowser').checked,
    videoButton: $('#videoButton').checked,
    videoButtonExclude: $('#videoButtonExclude').value.trim(),
    extensions: $('#extensions').value.trim(),
    minSizeMB: Math.max(0, Number($('#minSizeMB').value) || 0),
  };
}

async function save() {
  await chrome.storage.sync.set(read());
  await setToken(cleanToken($('#token').value));
  $('#saved').textContent = 'Saved.';
  setTimeout(() => { $('#saved').textContent = ''; }, 1500);
}

async function main() {
  const s = await getSettings();
  $('#token').value = await getToken();
  $('#port').value = s.port;
  $('#enabled').checked = s.enabled;
  $('#captureAll').checked = s.captureAll;
  $('#askInBrowser').checked = s.askInBrowser;
  $('#videoButton').checked = s.videoButton;
  $('#videoButtonExclude').value = s.videoButtonExclude;
  $('#extensions').value = s.extensions;
  $('#minSizeMB').value = s.minSizeMB;

  $('#token').oninput = checkToken;
  checkToken();
  $('#save').onclick = save;
  $('#test').onclick = async () => {
    await save();
    $('#token').value = cleanToken($('#token').value);
    checkToken();
    $('#result').textContent = 'Testing…';
    try {
      const info = await pingRdm();
      $('#result').textContent = `Connected to Flash ${info.version}.`;
    } catch (e) {
      $('#result').textContent = e.message;
    }
  };
}

main();
