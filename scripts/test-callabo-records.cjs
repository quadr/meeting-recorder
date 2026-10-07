const { test } = require('node:test');
const assert = require('node:assert/strict');
const { createCallaboRecords } = require('../ui/callabo-records.js');
class Node {
  constructor(tag) { this.tag = tag; this.children = []; this.listeners = {}; this.attrs = {}; this.className = ''; this.classList = { add: c => this.className += ` ${c}` }; }
  append(...nodes) { this.children.push(...nodes); }
  prepend(node) { this.children.unshift(node); }
  addEventListener(event, action) { this.listeners[event] = action; }
  setAttribute(name, value) { this.attrs[name] = value; }
}
const nodes = root => [root, ...root.children.flatMap(nodes)];
test('cards omit body previews and show only the explicit one-line summary until expanded', () => {
  const h = harness(), r = item([link(1, {status:'ready', summary:'# Title\n\nLong body.'})]);
  assert(!nodes(h.ui.render(r)).some(n => n.className === 'callabo-summary-preview'));
  r.callabo_links[0].remote.summary = '# Title\n\n## 한줄 요약\n**짧은** 요약.\n\n## Details\nLong body.';
  const all = nodes(h.ui.render(r));
  const preview = all.find(n => n.className === 'callabo-summary-preview');
  const detail = all.find(n => n.tag === 'details');
  assert.equal(preview.textContent, '짧은 요약.'); assert.equal(preview.hidden, false);
  detail.open = true; detail.listeners.toggle(); assert.equal(preview.hidden, true);
  assert(nodes(h.ui.render(r)).find(n => n.tag === 'details').open);
});
const link = (id, remote = null) => ({ uuid: `u${id}`, record_id: id, workspace: 'team', workspace_name: 'Team', url: `https://callabo.ai/en/workspace/team/record/${id}/detail`, remote });
const item = links => ({ folder:'2026-10', name:'meeting', mic:true, system:true, callabo_links:links });
function harness(invoke) {
  let time = 100_000;
  const calls = [], uploads = new Map(), store = new Map();
  const doc = { hidden:false, createElement: tag => new Node(tag) };
  const ui = createCallaboRecords({
    invoke: async (cmd,args) => { calls.push({cmd,args}); return invoke ? invoke(cmd,args) : link(1,{status:'processing'}); },
    t: (key) => key, refresh:async () => {}, upload:async () => {}, uploads,
    keyOf:(f,b) => `${f}::${b}`, doc, now:() => time,
    storage:{getItem:k=>store.get(k),setItem:(k,v)=>store.set(k,v)},
  });
  return {ui,calls,uploads,doc,store,advance:ms=>time+=ms};
}
test('latest link is the default; choosing another changes title without renaming the file', () => {
  const h = harness(); const r = item([link(1,{title:'First'}),link(2,{title:'Second'})]);
  assert.equal(h.ui.title(r),'Second'); h.ui.choose(r,'u1');
  assert.equal(h.ui.title(r),'First'); assert.equal(r.name,'meeting');
  assert.equal(h.store.get('callabo.primary.u1'),'u1');
  r.name='renamed'; assert.equal(h.ui.title(r),'First');
});
test('upload, remote processing, failed lookup and completion are distinct', () => {
  const h = harness(), r = item([link(1)]);
  assert.equal(h.ui.state(r),'checking');
  h.uploads.set('2026-10::meeting','uploading'); assert.equal(h.ui.state(r),'uploading');
  h.uploads.clear(); r.callabo_links[0].remote={status:'processing'}; assert.equal(h.ui.state(r),'processing');
  r.callabo_links[0].remote={status:'ready',title:'Cached',summary:'Saved',error:'Offline'};
  assert.equal(h.ui.state(r),'unavailable'); assert.equal(h.ui.title(r),'Cached');
  delete r.callabo_links[0].remote.error; assert.equal(h.ui.state(r),'ready');
  h.ui.uploadFailed(r,'Transfer failed'); assert.equal(h.ui.state(r),'uploadFailed');
  h.ui.uploadStarted(r); assert.equal(h.ui.state(r),'ready');
});
test('one request at a time; polling backs off and skips hidden or uploading records', async () => {
  let resolve;
  const h = harness(() => new Promise(done => resolve=done)), r = item([link(1)]);
  h.ui.setRecordings([r]); h.ui.setEnabled(true);
  const first=h.ui.tick();
  await h.ui.tick(); assert.equal(h.calls.length,1);
  resolve(link(1,{status:'processing'})); await first;
  await h.ui.tick(); assert.equal(h.calls.length,1);
  h.advance(15_000); h.doc.hidden=true; await h.ui.tick(); assert.equal(h.calls.length,1);
  h.doc.hidden=false; h.uploads.set('2026-10::meeting','uploading'); await h.ui.tick(); assert.equal(h.calls.length,1);
  h.uploads.clear(); const pending=h.ui.tick(); assert.equal(h.calls.length,2);
  resolve(link(1,{status:'ready'})); await pending;
  h.advance(16_000); await h.ui.tick(); assert.equal(h.calls.length,2);
});
test('enabling credentials does not poll a hidden tray window; older due records are not starved', async () => {
  const h=harness((cmd,args)=>link(Number(args.uuid.slice(1)),{status:'ready'}));
  const a=item([link(1)]), b={...item([link(2)]),name:'second'};
  h.ui.setRecordings([a,b]); h.ui.setEnabled(true); assert.equal(h.calls.length,0);
  await h.ui.tick(); h.advance(301_000); await h.ui.tick();
  assert.deepEqual(h.calls.map(c=>c.args.uuid),['u1','u2']);
});
test('lookup failures retain cached content and manual refresh can recover', async () => {
  let fail = true;
  const h = harness(() => { if(fail) throw new Error('Offline'); return link(1,{status:'ready',title:'Updated',summary:'New'}); });
  const r=item([link(1,{status:'ready',title:'Cached',summary:'Saved'})]);
  await h.ui.sync(r,r.callabo_links[0],true);
  assert.equal(h.ui.state(r),'unavailable'); assert.equal(h.ui.title(r),'Cached');
  fail=false; await h.ui.sync(r,r.callabo_links[0],true);
  assert.equal(h.ui.state(r),'ready'); assert.equal(h.ui.title(r),'Updated');
});
test('cards render a direct branded upload action, disabled progress, safe summary and every link', () => {
  const h=harness();
  const upload=nodes(h.ui.render(item([]))).find(n=>n.tag==='button');
  assert.equal(upload.textContent,'callabo.upload'); assert(upload.children.some(n=>n.tag==='img'));
  const r=item([link(1,{status:'ready',summary:'<script>alert(1)</script>'}),link(2,{status:'ready',summary:'Last'})]);
  h.ui.choose(r,'u1'); const all=nodes(h.ui.render(r));
  assert.equal(all.filter(n=>n.tag==='option').length,2);
  assert(all.some(n=>n.textContent==='<script>alert(1)</script>')); assert(!all.some(n=>n.tag==='script'));
  h.uploads.set('2026-10::meeting','uploading'); const busy=nodes(h.ui.render(r));
  assert(busy.some(n=>n.tag==='button' && n.disabled)); assert(busy.some(n=>n.attrs.role==='progressbar'));
  assert(!busy.some(n=>n.textContent==='callabo.upload'));
});
