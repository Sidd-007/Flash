// Where the pairing token lives. Run with: node --test "extension/tests/*.test.mjs"

import assert from 'node:assert/strict';
import fs from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const source = fs.readFileSync(new URL('../common.js', import.meta.url), 'utf8');

function area(data = {}) {
  return {
    data,
    get: async (key) => (key == null ? { ...data } : { [key]: data[key] }),
    set: async (values) => { Object.assign(data, values); },
    remove: async (key) => { delete data[key]; },
  };
}

/** common.js loaded against fake storage areas. */
function load({ sync = {}, local = {}, setAccessLevel } = {}) {
  const storage = { sync: area(sync), local: area(local) };
  if (setAccessLevel) storage.local.setAccessLevel = setAccessLevel;
  const context = vm.createContext({ chrome: { storage } });
  vm.runInContext(`${source}\n;globalThis.api = { getSettings, getToken, setToken, restrictLocalStorage };`, context);
  return { ...context.api, storage };
}

const OLD = 'a'.repeat(48);
const NEW = 'b'.repeat(48);

test('a synced token moves to local storage and leaves sync', async () => {
  const { getToken, storage } = load({ sync: { token: OLD } });
  assert.equal(await getToken(), OLD);
  assert.equal(storage.local.data.token, OLD);
  assert.ok(!('token' in storage.sync.data));
});

test('a synced copy is removed even when a local token already exists', async () => {
  const { getToken, storage } = load({ sync: { token: OLD }, local: { token: NEW } });
  assert.equal(await getToken(), NEW, 'the local token is the newer one');
  assert.ok(!('token' in storage.sync.data));
  assert.equal(storage.local.data.token, NEW);
});

test('an empty synced value is cleaned up too', async () => {
  const { getToken, storage } = load({ sync: { token: '' } });
  assert.equal(await getToken(), '');
  assert.ok(!('token' in storage.sync.data));
});

test('saving a token writes only to local storage', async () => {
  const { setToken, getSettings, storage } = load({ sync: { token: OLD, port: 47321 } });
  await setToken(NEW);
  assert.equal(storage.local.data.token, NEW);
  assert.ok(!('token' in storage.sync.data));
  assert.ok(!('token' in (await getSettings())), 'preferences never carry the token');
});

test('local storage is restricted to trusted extension contexts', async () => {
  const calls = [];
  const { restrictLocalStorage } = load({ setAccessLevel: async (options) => { calls.push(options); } });
  assert.equal(await restrictLocalStorage(), true);
  // Objects from the sandbox have its own prototypes, so compare the values.
  assert.deepEqual(JSON.parse(JSON.stringify(calls)), [{ accessLevel: 'TRUSTED_CONTEXTS' }]);
});

test('browsers without setAccessLevel for local storage keep working', async () => {
  assert.equal(await load().restrictLocalStorage(), false, 'method missing');
  const rejecting = load({ setAccessLevel: async () => { throw new Error('This StorageArea does not support setAccessLevel'); } });
  assert.equal(await rejecting.restrictLocalStorage(), false, 'method rejects');
});
