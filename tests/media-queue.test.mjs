import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';
const source = await readFile(new URL('../src/utils/mediaQueue.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 } });
const { createMediaQueue } = await import(`data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`);
const tick = () => new Promise(resolve => setImmediate(resolve));

test('queued images expire without starting another request', async () => {
  const queue = createMediaQueue(1, 2); let release; let ran = false;
  const running = queue.run('slow', () => new Promise(resolve => { release = resolve; }));
  const waiting = queue.run('waiting', async () => { ran = true; });
  const checked = assert.rejects(waiting, /排队超时/);
  await new Promise(resolve => setTimeout(resolve, 10));
  release(); await running; await checked;
  assert.equal(ran, false);
});

test('downloads have bounded concurrency and detail work can jump the queue', async () => {
  const queue = createMediaQueue(1); const started = []; let release;
  const first = queue.run('first', () => new Promise(resolve => { started.push('first'); release = resolve; }));
  const second = queue.run('second', async () => { started.push('second'); });
  const detail = queue.run('detail', async () => { started.push('detail'); });
  queue.promote('detail');
  await tick(); assert.deepEqual(started, ['first']);
  release(); await Promise.all([first, second, detail]);
  assert.deepEqual(started, ['first', 'detail', 'second']);
});

test('leaving a page rejects queued work without starting its network request', async () => {
  const queue = createMediaQueue(1); let release; let ran = false;
  const running = queue.run('running', () => new Promise(resolve => { release = resolve; }));
  const pending = queue.run('pending', async () => { ran = true; });
  const rejected = assert.rejects(pending, /取消/);
  queue.clearPending(); release(); await running; await rejected;
  assert.equal(ran, false);
  assert.equal(await queue.run('next-page', async () => 42), 42);
});
