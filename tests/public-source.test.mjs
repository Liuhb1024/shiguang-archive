import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';

test('public settings retain attribution without inherited payment QR codes', async () => {
  const source = await readFile(new URL('../src/views/SettingsView.vue', import.meta.url), 'utf8');
  assert.match(source, /基于 QzoneArchive/);
  assert.doesNotMatch(source, /sponsorImages|hideMissingSponsorCode|收款码|\/sponsor\//);
});
