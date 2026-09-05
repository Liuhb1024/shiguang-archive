import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';
const transpile = async file => {
  const source = await readFile(new URL(file, import.meta.url), 'utf8');
  const { outputText } = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 } });
  return import(`data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`);
};
const { contentEvidence } = await transpile('../src/utils/contentEvidence.ts');
const { parseQzoneText } = await transpile('../src/utils/qzoneText.ts');

test('ellipsis is a possible summary signal, never a verified completeness claim', () => {
  for (const text of ['很长的记录...', '另一条…  ']) assert.equal(contentEvidence(text).possibleSummary, true);
  assert.equal(contentEvidence('正文没有省略号').possibleSummary, false);
  assert.equal(contentEvidence('📷记忆').characters, 3);
});
test('the text renderer retains every character of long source text', () => {
  const text = '这是一段本地合成长文。\n'.repeat(400) + '全文末尾核验标记';
  assert.equal(parseQzoneText(text).map(part => part.value).join(''), text);
});
