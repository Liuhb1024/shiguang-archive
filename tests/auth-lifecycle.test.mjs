import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createPinia, defineStore } from 'pinia';
import { computed, ref } from 'vue';
import ts from 'typescript';

const source = await readFile(new URL('../src/stores/auth.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 } });
// Exercise the real store with Vue/Pinia; replace only the native IPC boundary.
const makeStore = new Function('invoke', 'defineStore', 'computed', 'ref', outputText
  .replace(/^import .*;\r?\n/gm, '')
  .replace('export const useAuthStore =', 'return'));
const fixture = invoke => makeStore(invoke, defineStore, computed, ref)(createPinia());
const deferred = () => { let resolve; let reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; };

test('closing a pending QR request permits reopening and ignores the old response', async () => {
  const requests = [];
  const auth = fixture(async command => {
    assert.equal(command, 'start_qr_login');
    const request = deferred(); requests.push(request); return request.promise;
  });
  const first = auth.openLogin();
  assert.equal(auth.loading, true);
  auth.closeLogin();
  assert.equal(auth.loading, false);
  const second = auth.openLogin();
  assert.equal(requests.length, 2);
  requests[0].resolve({ qrImage: 'old-synthetic-qr' });
  await first;
  assert.equal(auth.qrImage, '');
  assert.equal(auth.loading, true);
  requests[1].reject('synthetic login failure');
  await second;
  assert.equal(auth.status, 'error');
  assert.equal(auth.loading, false);
});

test('logout immediately hides identity and ignores a late session response', async () => {
  const status = deferred(); const logout = deferred();
  const auth = fixture(command => command === 'get_login_status' ? status.promise : logout.promise);
  auth.user = { uin: '10001', nickname: 'Synthetic A' }; auth.status = 'success';
  const restore = auth.restoreSession();
  const version = auth.sessionVersion;
  const quitting = auth.logout();
  assert.equal(auth.loggedIn, false);
  assert.equal(auth.user, undefined);
  assert.ok(auth.sessionVersion > version);
  status.resolve({ status: 'success', user: { uin: '10001', nickname: 'Synthetic A' } });
  await restore;
  assert.equal(auth.user, undefined);
  logout.resolve(); await quitting;
  assert.equal(auth.loggedIn, false);
});
