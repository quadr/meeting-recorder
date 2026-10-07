const { test } = require('node:test');
const assert = require('node:assert/strict');
const { renderMarkdown, oneLineSummary } = require('../ui/summary-markdown.js');
class Node {
  constructor(tag) { this.tag = tag; this.children = []; this.attrs = {}; this.listeners = {}; }
  append(...nodes) { this.children.push(...nodes); }
  setAttribute(key, value) { this.attrs[key] = value; }
  addEventListener(key, fn) { this.listeners[key] = fn; }
}
const doc = { createElement: tag => new Node(tag) };
const nodes = root => [root, ...root.children.flatMap(nodes)];
const text = node => (node.textContent ?? '') + node.children.map(text).join('');
test('only an explicitly labeled one-line section becomes a plain-text teaser', () => {
  assert.equal(oneLineSummary('# Title\n\nLong body preview.'), null);
  assert.equal(oneLineSummary('# Title\n\n## Summary\nLong body.'), null);
  assert.equal(oneLineSummary('# Title\n\n## 한줄 요약\n**큐** &amp; [워커](https://example.com)를\n분리한다.\n\n## 결정\nDetails'), '큐 & 워커를 분리한다.');
  assert.equal(oneLineSummary('## 한 줄 요약\n\n## Empty section'), null);
  assert.equal(oneLineSummary('## TL;DR\nKeep it short.'), 'Keep it short.');
});
test('Markdown becomes semantic headings, nested lists, emphasis, code, quotes and tables', () => {
  const root = renderMarkdown('# 제목\n\n## 결정\n**중요** *강조* ~~취소~~ `x < y`\n\n3. 첫째\n   - 하위\n\n- [x] 완료\n\n> 인용\n\n```js\nx < y\n```\n\n| 담당 | 작업 |\n| --- | --- |\n| 팀 | 검토 |', { doc });
  const all = nodes(root), tags = all.map(n => n.tag);
  for (const tag of ['h1','h2','strong','em','del','code','pre','ol','ul','li','blockquote','table','th','td']) assert(tags.includes(tag), tag);
  assert.equal(all.find(n => n.tag === 'ol').attrs.start, '3');
  assert(text(root).includes('☑ 완료'));
  assert(text(root).includes('x < y'));
  assert(!text(root).includes('**'));
});
test('remote HTML, images and unsafe links stay inert; safe links use the external opener', () => {
  const opened = [];
  const root = renderMarkdown('<script>alert(1)</script>\n\n![image](https://example.com/track.png) [bad](javascript:alert%281%29) [encoded](javascript&#58;alert) [safe](https://example.com?a=1&amp;b=2)', { doc, openLink: url => opened.push(url) });
  const all = nodes(root);
  assert(!all.some(n => ['script','img','iframe'].includes(n.tag)));
  const links = all.filter(n => n.tag === 'a');
  assert.equal(links.length, 1);
  let prevented = false;
  links[0].listeners.click({ preventDefault: () => prevented = true, stopPropagation() {} });
  assert(prevented); assert.deepEqual(opened, ['https://example.com?a=1&b=2']);
});
