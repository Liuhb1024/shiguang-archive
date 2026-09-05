import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

const source = await readFile(new URL('../src/utils/mediaFailure.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 } });
const { mediaFailure } = await import(`data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`);

test('rate limits and expired links are not repeatedly requested by retry button', () => {
  for (const reason of ['HTTP 429', 'HTTP 403', 'QQ 返回了图片不存在占位图', '操作已取消', '尚未登录 QQ 空间']) {
    assert.equal(mediaFailure(reason).retryable, false, reason);
  }
});
test('network, policy and storage failures are distinguishable', () => {
  assert.match(mediaFailure('QQ 媒体请求超时').title, /网络/);
  assert.match(mediaFailure('不允许的请求域名').title, /安全/);
  assert.match(mediaFailure('写入媒体失败，请检查磁盘空间').title, /保存/);
});
test('unknown errors never echo sensitive raw values', () => {
  const failure = mediaFailure('https://example.invalid/?token=private-fixture');
  assert.doesNotMatch(JSON.stringify(failure), /private-fixture|example.invalid/);
});
