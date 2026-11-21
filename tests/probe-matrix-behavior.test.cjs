/* 仅用 Node VM 加载 ui/app.js 的真实函数；不启动浏览器，也不连接实际 IPC。 */
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const source = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8').replace(/\r\n/g, '\n');

function section(start, end) {
  const from = source.indexOf(start);
  assert.notEqual(from, -1, `真实源码标记缺失：${start}`);
  const to = source.indexOf(end, from);
  assert.ok(to > from, `真实源码结束标记缺失：${end}`);
  return source.slice(from, to);
}
const domHelpers = section('function el(tag, attrs', '/* 隐蔽提示');
const matrixFunctions = section('/* 旧「请求」入口', '\nfunction iconBtn(');
const catalogFunctions = section('const SITE_MODELS_BUSY', '/* API 页左菜单');
const matrixBindings = section("  $('mx-close').addEventListener", "  ['s-name', 's-home']");
const keyboardBindings = section("  document.addEventListener('keydown', (e) => {\n    if (e.key === 'Escape') {", '\n}\n\nfunction hideToTray');
const clearFunction = source.match(/function clear\(node\) \{[^\n]+\}/)?.[0];
assert.ok(clearFunction, '真实 DOM 清理函数缺失');

class FakeElement {
  constructor(tag, doc, text = '') {
    this.tagName = tag.toUpperCase();
    this.ownerDocument = doc;
    this.childNodes = [];
    this.attributes = new Map();
    this.listeners = new Map();
    this.dataset = {};
    this.className = '';
    this.value = '';
    this.disabled = false;
    this.hidden = false;
    this._text = text;
  }
  get firstChild() { return this.childNodes[0] || null; }
  get textContent() { return this._text + this.childNodes.map((c) => c.textContent).join(''); }
  set textContent(value) { this._text = String(value); this.childNodes = []; }
  appendChild(node) { node.parentNode = this; this.childNodes.push(node); return node; }
  removeChild(node) {
    const at = this.childNodes.indexOf(node);
    assert.notEqual(at, -1);
    this.childNodes.splice(at, 1); node.parentNode = null; return node;
  }
  setAttribute(name, value) { this.attributes.set(name, String(value)); }
  getAttribute(name) { return this.attributes.get(name) ?? null; }
  addEventListener(name, fn) {
    if (!this.listeners.has(name)) this.listeners.set(name, []);
    this.listeners.get(name).push(fn);
  }
  focus() { this.ownerDocument.activeElement = this; }
  async dispatch(name, extra = {}) {
    if (name === 'click' && this.disabled) return;
    const event = { target: this, currentTarget: this, preventDefault() {}, stopPropagation() {}, ...extra };
    await Promise.all((this.listeners.get(name) || []).map((fn) => fn(event)));
  }
}
function descendants(node) { return [node, ...node.childNodes.flatMap(descendants)]; }
function fakeDocument() {
  const ids = new Map();
  const elements = [];
  const listeners = new Map();
  const doc = {
    activeElement: null,
    createElement(tag) { const n = new FakeElement(tag, doc); elements.push(n); return n; },
    createTextNode(text) { return new FakeElement('#text', doc, String(text)); },
    getElementById(id) {
      if (!ids.has(id)) { const n = doc.createElement('div'); n.id = id; n.hidden = true; ids.set(id, n); }
      return ids.get(id);
    },
    querySelectorAll(selector) {
      if (selector === '[data-models-site]') return elements.filter((n) => n.getAttribute('data-models-site') !== null);
      throw new Error(`未模拟选择器：${selector}`);
    },
    addEventListener(name, fn) {
      if (!listeners.has(name)) listeners.set(name, []);
      listeners.get(name).push(fn);
    },
    dispatch(name, event) { for (const fn of listeners.get(name) || []) fn(event); },
  };
  return doc;
}
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
const plain = (value) => JSON.parse(JSON.stringify(value));
const endpoint = (url, fmt = 'openai') => `${fmt.trim()}|${url.trim().replace(/\/+$/, '')}`;
const E1 = endpoint('https://first.invalid/v1');
const E2 = endpoint('https://second.invalid/v1');
function catalog(url, models, state = 'ok', fmt = 'openai') {
  return { endpoint_id: endpoint(url, fmt), fmt, url, models, state, msg: state === 'error' ? '目录返回 403' : '', checked_at: '2026-11-21T10:00:00Z' };
}
function fixture(id = 'A') {
  return {
    id, name: `站点${id}`, models_list: 'site-suggestion',
    base_urls: [{ format: 'openai', url: 'https://first.invalid/v1' }, { format: 'openai', url: 'https://second.invalid/v1' }],
    keys: [
      { id: `${id}-k1`, label: '密钥一', models_list: '', model_catalogs: [
        catalog('https://first.invalid/v1', ['first-one', 'first-two']),
        catalog('https://second.invalid/v1', ['second-one']),
      ] },
      { id: `${id}-k2`, label: '密钥二', models_list: 'legacy-model', model_catalogs: [
        catalog('https://first.invalid/v1', ['key-two']),
        catalog('https://second.invalid/v1', [], 'empty'),
      ] },
    ],
  };
}
function reply(payload, state = 'ok') {
  return { ok: true, msg: JSON.stringify({ cells: payload.choices.map((c) => {
    const split = c.endpoint.indexOf('|');
    return { ...c, fmt: c.endpoint.slice(0, split), url: c.endpoint.slice(split + 1), model: c.model,
      state, code: 200, ms: 25, msg: '合成响应', checked_at: '2026-11-21T11:00:00Z' };
  }) }) };
}
function harness(sites = [fixture()], onInvoke = (_command, { payload }) => payload.action === 'key_matrix' ? reply(payload) : { ok: true, msg: '已取消' }) {
  const document = fakeDocument(), calls = [], toasts = [];
  let loads = 0;
  const sandbox = {
    document, S: { data: { keys: sites }, view: 'keys' },
    $: (id) => document.getElementById(id),
    invoke(command, args) { calls.push({ command, ...plain(args) }); return onInvoke(command, args); },
    toast(...args) { toasts.push(args); },
    load: async () => { loads += 1; },
    BASE_FMT: [['openai', 'Chat Completions'], ['anthropic', 'Messages']],
    copyText() {}, openSiteModal() {}, renderKeys() {},
  };
  const context = vm.createContext(sandbox);
  vm.runInContext(`${domHelpers}\n${clearFunction}\n${matrixFunctions}\n${catalogFunctions}\n${matrixBindings}\n${keyboardBindings}\n
    globalThis.api = { MX, SITE_MODELS_BUSY, mxBases, mxEndpointId, mxChoice, mxGet, mxChoiceKey,
      mxSetModel, mxKeyModels, openMatrix, closeMatrix, mxCancel, mxRun, renderMatrix,
      keyChat, siteInfo, pullSiteModelsAll };
    function chip(text, kind) { return el('span', { class: kind }, text); }
  `, context, { filename: 'app.js:真实矩阵函数' });
  const api = sandbox.api;
  const cell = (kid = 'A-k1', ep = E1, model) => {
    const selected = model ?? api.MX.choices.get(api.mxChoiceKey(kid, ep));
    return api.mxGet(api.MX.siteId, kid, ep, selected?.trim() || '');
  };
  const view = (kid = 'A-k1', ep = E1) => api.MX.views.get(api.mxChoiceKey(kid, ep));
  const requests = () => calls.filter((c) => c.payload.action === 'key_matrix');
  return { api, document, calls, toasts, sites, state: sandbox.S, cell, view, requests, get loads() { return loads; } };
}

test('同协议不同URL各列保留，密钥额外地址合并且优先 base_format', () => {
  const site = fixture();
  site.base_urls.push({ format: ' openai ', url: ' https://first.invalid/v1/// ' });
  site.keys[0].base_url = 'https://extra.invalid/custom';
  site.keys[0].base_format = 'anthropic';
  const h = harness([site]);
  const bases = plain(h.api.mxBases(site));
  assert.equal(bases.length, 3);
  assert.deepEqual(bases.map((b) => b.id), [E1, E2, endpoint('https://extra.invalid/custom', 'anthropic')]);
  assert.equal(h.api.mxEndpointId(' openai ', ' https://first.invalid/v1/// '), E1);
  h.api.openMatrix('A');
  const head = h.document.getElementById('mx-head').textContent;
  for (const b of bases) assert.ok(head.includes(b.url.replace(/^https?:\/\//, '')));
  assert.equal(h.api.MX.views.size, 5, '专属地址只能由所属密钥使用');
  assert.ok(h.document.getElementById('mx-body').textContent.includes('不适用'));
  assert.equal(h.requests().length, 0, '打开矩阵不能自动发送计费请求');
});

test('单格真实按钮只发送该密钥、精确端点、当前模型和请求ID，无明文密钥', async () => {
  const site = fixture();
  site.keys[0].key = 'SYNTHETIC_NOT_A_REAL_CREDENTIAL';
  const h = harness([site]);
  h.api.keyChat('A', 'A-k1');
  assert.deepEqual(plain(h.api.MX.focus), { type: 'key', value: 'A-k1' });
  const v = h.view();
  v.input.value = 'first-two';
  await v.input.dispatch('input');
  const button = descendants(v.input.parentNode).find((n) => n.tagName === 'BUTTON' && n.textContent === '检测');
  assert.ok(button);
  await button.dispatch('click');
  assert.equal(h.requests().length, 1);
  const p = h.requests()[0].payload;
  assert.deepEqual(p.keys, ['A-k1']);
  assert.deepEqual(p.endpoints, [E1]);
  assert.deepEqual(p.choices, [{ key: 'A-k1', endpoint: E1, model: 'first-two' }]);
  assert.equal(p.site, 'A'); assert.ok(p.request_id);
  assert.deepEqual(Object.keys(p).sort(), ['action', 'choices', 'endpoints', 'keys', 'request_id', 'site']);
  assert.ok(!JSON.stringify(p).includes(site.keys[0].key));
  assert.equal(h.cell().state, 'ok');
  assert.match(v.model.textContent, /first-two/);
  assert.match(v.status.title, /https:\/\/first\.invalid\/v1/);
  assert.doesNotMatch(v.time.textContent, /尚未检测/);
});

test('每Key每端点独立目录优先预填；空、错误、取消、过期目录不回退站点或历史首模型', () => {
  const site = fixture();
  for (const state of ['error', 'cancelled', 'stale']) {
    site.keys.push({ id: state, label: state, models_list: 'must-not-select', model_catalogs: [catalog('https://first.invalid/v1', [], state)] });
  }
  site.keys.push({ id: 'missing', label: '缺目录', models_list: '' });
  site.keys.push({ id: 'manual', label: '历史', models_list: 'manual-one,manual-two' });
  const h = harness([site]); h.api.openMatrix('A');
  assert.equal(h.view('A-k1', E1).input.value, 'first-one');
  assert.equal(h.view('A-k1', E2).input.value, 'second-one');
  assert.equal(h.view('A-k2', E1).input.value, 'key-two');
  assert.equal(h.view('A-k2', E2).input.value, '');
  for (const id of ['error', 'cancelled', 'stale', 'missing']) assert.equal(h.view(id, E1).input.value, '');
  assert.equal(h.view('manual', E1).input.value, 'manual-one');
  const list = descendants(h.view('missing', E1).input.parentNode).find((n) => n.tagName === 'DATALIST');
  const suggestion = list.childNodes.find((n) => n.value === 'site-suggestion');
  assert.ok(suggestion); assert.match(suggestion.label, /未验证/);
  assert.ok(h.document.getElementById('mx-body').textContent.includes('3 个模型（按密钥汇总）'));
});

test('缺模型格不请求，已有输入继续；整行和整列过滤使用endpoint而非fmt', async () => {
  const h = harness(); h.api.openMatrix('A');
  await h.api.mxRun({ keys: ['A-k2'], endpoints: [E2] });
  assert.equal(h.requests().length, 0); assert.equal(h.cell('A-k2', E2).state, 'unconfigured');
  await h.api.mxRun({ keys: ['A-k2'] });
  assert.deepEqual(h.requests()[0].payload.choices, [{ key: 'A-k2', endpoint: E1, model: 'key-two' }]);
  await h.api.mxRun({ endpoints: [E2] });
  const payload = h.requests()[1].payload;
  assert.deepEqual(payload.endpoints, [E2]);
  assert.deepEqual(payload.choices, [{ key: 'A-k1', endpoint: E2, model: 'second-one' }]);
  assert.equal(h.cell('A-k2', E2).state, 'unconfigured');
});

test('关闭后更换Key重开，无旧绿色结果和旧模型残留', async () => {
  const h = harness(); h.api.openMatrix('A');
  await h.api.mxRun({ keys: ['A-k1'], endpoints: [E1] });
  assert.equal(h.cell().state, 'ok');
  await h.document.getElementById('mx-close').dispatch('click');
  assert.equal(h.api.MX.cells.size, 0);
  h.sites[0].keys[0] = { id: 'replacement', label: '新密钥', models_list: 'new-model' };
  h.api.openMatrix('A', { type: 'key', value: 'replacement' });
  assert.equal(h.api.MX.cells.size, 0);
  assert.equal(h.view('replacement').input.value, 'new-model');
  assert.equal(h.cell('replacement').state, 'unt');
  assert.doesNotMatch(h.document.getElementById('mx-body').textContent, /合成响应/);
});

test('A在途关闭打开B即可检测；A响应与finally不能覆盖B结果或busy', async () => {
  const a = deferred(), b = deferred();
  const h = harness([fixture('A'), fixture('B')], (_cmd, { payload }) => {
    if (payload.action === 'key_matrix_cancel') return { ok: true, msg: '取消已登记' };
    return payload.site === 'A' ? a.promise : b.promise;
  });
  h.api.openMatrix('A');
  const old = h.api.mxRun({ keys: ['A-k1'], endpoints: [E1] });
  const oldPayload = h.requests()[0].payload;
  h.api.closeMatrix(); h.api.openMatrix('B');
  const next = h.api.mxRun({ keys: ['B-k1'], endpoints: [E1] });
  assert.equal(h.requests().length, 2); assert.equal(h.api.MX.busy, true);
  const cancel = h.calls.find((c) => c.payload.action === 'key_matrix_cancel').payload;
  assert.deepEqual(cancel, { action: 'key_matrix_cancel', request_id: oldPayload.request_id });
  a.resolve(reply(oldPayload)); await old;
  assert.equal(h.api.MX.siteId, 'B'); assert.equal(h.api.MX.busy, true);
  assert.equal(h.cell('B-k1').state, 'run');
  b.resolve(reply(h.requests()[1].payload)); await next;
  assert.equal(h.api.MX.busy, false); assert.equal(h.cell('B-k1').state, 'ok');
  assert.ok([...h.api.MX.cells.values()].every((c) => c.key.startsWith('B-')));
});

test('取消停止后续调度并忽略旧响应，同站可以立即重新检测且忽略旧finally', async () => {
  const oldReply = deferred(), newReply = deferred(); let count = 0;
  const h = harness([fixture()], (_cmd, { payload }) => {
    if (payload.action === 'key_matrix_cancel') return { ok: true, msg: '取消已登记' };
    return ++count === 1 ? oldReply.promise : newReply.promise;
  });
  h.api.openMatrix('A');
  const old = h.api.mxRun({ keys: ['A-k1'], endpoints: [E1] });
  const oldPayload = h.requests()[0].payload;
  await h.document.getElementById('mx-cancel').dispatch('click');
  assert.equal(h.api.MX.busy, false); assert.equal(h.cell().state, 'cancelled');
  assert.match(h.toasts.at(-1)[0], /停止后续调度/);
  const next = h.api.mxRun({ keys: ['A-k1'], endpoints: [E1] });
  const nextPayload = h.requests()[1].payload;
  assert.notEqual(nextPayload.request_id, oldPayload.request_id);
  oldReply.resolve(reply(oldPayload, 'key_denied')); await old;
  assert.equal(h.api.MX.busy, true); assert.equal(h.cell().state, 'run');
  newReply.resolve(reply(nextPayload)); await next;
  assert.equal(h.cell().state, 'ok'); assert.equal(h.api.MX.busy, false);
});

test('取消后即使不再检测，旧成功响应也不能恢复绿色', async () => {
  const d = deferred();
  const h = harness([fixture()], (_cmd, { payload }) => payload.action === 'key_matrix' ? d.promise : { ok: true });
  h.api.openMatrix('A'); const work = h.api.mxRun({ keys: ['A-k1'], endpoints: [E1] });
  const payload = h.requests()[0].payload;
  h.api.mxCancel(); d.resolve(reply(payload)); await work;
  assert.equal(h.cell().state, 'cancelled'); assert.equal(h.api.MX.busy, false);
});

test('改模型只清本格结果，结果刷新和render不会重建输入或丢失焦点', async () => {
  const h = harness(); h.api.openMatrix('A');
  await h.api.mxRun({ keys: ['A-k1'] });
  const input = h.view().input; input.focus(); input.value = ' user-entered ';
  await input.dispatch('input'); h.api.renderMatrix();
  assert.equal(h.cell().state, 'unt'); assert.equal(h.cell('A-k1', E2).state, 'ok');
  assert.equal(h.view().input, input); assert.equal(h.document.activeElement, input);
  assert.equal(input.value, ' user-entered ');
  await h.api.mxRun({ keys: ['A-k1'], endpoints: [E1] });
  assert.equal(h.requests().at(-1).payload.choices[0].model, 'user-entered');
  assert.equal(h.view().input, input); assert.equal(input.value, ' user-entered ');
});

test('在途改模型再改回原值，旧响应仍失效，其他格不受影响', async () => {
  const d = deferred(); const h = harness([fixture()], (_cmd, { payload }) => payload.action === 'key_matrix' ? d.promise : { ok: true });
  h.api.openMatrix('A'); const work = h.api.mxRun({ keys: ['A-k1'] });
  const p = h.requests()[0].payload;
  const input = h.view().input; input.value = 'changed'; await input.dispatch('input');
  input.value = 'first-one'; await input.dispatch('input');
  d.resolve(reply(p)); await work;
  assert.equal(h.cell().state, 'unt'); assert.equal(h.cell('A-k1', E2).state, 'ok');
  assert.equal(h.api.MX.busy, false);
});

test('密钥或URL数据变更使旧结果失效，模型输入受保护', async () => {
  const h = harness(); h.api.openMatrix('A'); await h.api.mxRun({ keys: ['A-k1'] });
  const input = h.view().input; input.value = 'user-model'; await input.dispatch('input');
  h.sites[0].keys[0].masked = '已更换的打码值'; h.api.renderMatrix();
  assert.equal(h.api.MX.cells.size, 0); assert.equal(h.view().input.value, 'user-model');
  h.sites[0].base_urls[0].url = 'https://changed.invalid/v1'; h.api.renderMatrix();
  assert.equal(h.api.MX.cells.size, 0);
  assert.ok([...h.api.MX.views.values()].some((v) => v.b.url === 'https://changed.invalid/v1'));
});

test('在途重复提交被busy拦截，所有检测按钮禁用但仍可修改输入', async () => {
  const d = deferred(); const h = harness([fixture()], (_cmd, { payload }) => payload.action === 'key_matrix' ? d.promise : { ok: true });
  h.api.openMatrix('A'); const work = h.api.mxRun({ keys: ['A-k1'], endpoints: [E1] });
  await h.api.mxRun({});
  assert.equal(h.requests().length, 1);
  assert.equal(h.document.getElementById('mx-all').disabled, true);
  assert.ok(h.api.MX.buttons.every((b) => b.disabled)); assert.equal(h.view().input.disabled, false);
  d.resolve(reply(h.requests()[0].payload)); await work;
  assert.equal(h.document.getElementById('mx-all').disabled, false);
  assert.ok(h.api.MX.buttons.every((b) => !b.disabled));
});

for (const [name, handler, expected] of [
  ['HTTP业务失败', () => ({ ok: false, msg: 'HTTP 502' }), 'error'],
  ['IPC同步异常', () => { throw new Error('IPC断开'); }, 'error'],
  ['IPC异步异常', () => Promise.reject(new Error('IPC拒绝')), 'error'],
  ['JSON非法', () => ({ ok: true, msg: '{invalid' }), 'invalid_response'],
  ['cells形状非法', () => ({ ok: true, msg: '{"cells":null}' }), 'invalid_response'],
  ['结果缺失', () => ({ ok: true, msg: '{"cells":[]}' }), 'invalid_response'],
  ['模型被偷换', (p) => { const r = reply(p); const d = JSON.parse(r.msg); d.cells[0].model = 'swapped'; return { ok: true, msg: JSON.stringify(d) }; }, 'invalid_response'],
  ['地址不匹配', (p) => { const r = reply(p); const d = JSON.parse(r.msg); d.cells[0].url = 'https://wrong.invalid/v1'; return { ok: true, msg: JSON.stringify(d) }; }, 'invalid_response'],
]) {
  test(`${name}后busy和按钮恢复，且不显示旧成功`, async () => {
    const h = harness([fixture()], (_cmd, { payload }) => handler(payload));
    h.api.openMatrix('A'); await h.api.mxRun({ keys: ['A-k1'], endpoints: [E1] });
    assert.equal(h.api.MX.busy, false); assert.equal(h.api.MX.request, null);
    assert.equal(h.document.getElementById('mx-all').disabled, false);
    assert.equal(h.cell().state, expected);
  });
}

test('Escape优先关闭矩阵并取消检测，不切换后台页面', async () => {
  const d = deferred(); const h = harness([fixture()], (_cmd, { payload }) => payload.action === 'key_matrix' ? d.promise : { ok: true });
  h.api.openMatrix('A'); const work = h.api.mxRun({ keys: ['A-k1'], endpoints: [E1] });
  const p = h.requests()[0].payload; let prevented = false, stopped = false;
  h.document.dispatch('keydown', { key: 'Escape', preventDefault() { prevented = true; }, stopPropagation() { stopped = true; } });
  assert.equal(h.document.getElementById('mxmodal').hidden, true);
  assert.equal(h.api.MX.busy, false); assert.ok(prevented && stopped);
  assert.equal(h.state.view, 'keys');
  assert.equal(h.calls.filter((c) => c.payload.action === 'key_matrix_cancel').length, 1);
  d.resolve(reply(p)); await work; assert.equal(h.api.MX.cells.size, 0);
});

test('只有密钥地址的站点仍有唯一获取入口、逐Key目录与失败说明，保留历史汇总', async () => {
  const site = fixture(); site.base_urls = []; site.models_list = 'historical-site-model';
  site.keys[0].base_url = 'https://first.invalid/v1';
  site.keys[0].model_catalogs[0].state = 'error'; site.keys[0].model_catalogs[0].msg = '目录返回 403';
  const h = harness([site]); const info = h.api.siteInfo(site);
  assert.ok(info);
  const fetch = descendants(info).filter((n) => n.tagName === 'BUTTON' && n.textContent === '获取模型');
  assert.equal(fetch.length, 1); assert.match(info.textContent, /汇总不代表每把密钥权限/);
  assert.match(info.textContent, /密钥一/); assert.match(info.textContent, /获取失败/); assert.match(info.textContent, /目录返回 403/);
  assert.match(info.textContent, /获取时间/); assert.match(info.textContent, /historical-site-model/);
  await fetch[0].dispatch('click');
  assert.deepEqual(h.calls[0].payload, { action: 'site_models_all', site: 'A' });
  assert.equal(site.models_list, 'historical-site-model'); assert.equal(h.loads, 1);
});

test('接入点行入口聚焦精确endpoint，同协议第二个地址不落到第一列', async () => {
  const h = harness();
  const info = h.api.siteInfo(h.sites[0]);
  const row = descendants(info).find((n) => n.className === 'sirow' && n.textContent.includes('https://second.invalid/v1'));
  const button = descendants(row).find((n) => n.tagName === 'BUTTON' && n.textContent === '连通检测');
  await button.dispatch('click');
  assert.deepEqual(plain(h.api.MX.focus), { type: 'endpoint', value: E2 });
  assert.equal(h.requests().length, 0);
  const focused = descendants(h.document.getElementById('mx-head')).filter((n) => n.tagName === 'TH' && n.className === 'hl');
  assert.equal(focused.length, 1); assert.match(focused[0].textContent, /second\.invalid/);
});

test('新目录到达只更新未编辑的默认模型，保留已编辑输入并清除旧结果', () => {
  const site = fixture(); site.keys[0].model_catalogs = [];
  const h = harness([site]); h.api.openMatrix('A');
  assert.equal(h.view().input.value, '');
  h.api.mxSetModel('A-k1', E2, 'keep-user-model');
  site.keys[0].model_catalogs = [catalog('https://first.invalid/v1', ['newly-fetched']), catalog('https://second.invalid/v1', ['must-not-overwrite'])];
  h.api.renderMatrix();
  assert.equal(h.view().input.value, 'newly-fetched');
  assert.equal(h.view('A-k1', E2).input.value, 'keep-user-model');
  assert.equal(h.api.MX.cells.size, 0);
});

test('后台数据变更会取消在途请求并忽略旧响应', async () => {
  const d = deferred(); const h = harness([fixture()], (_cmd, { payload }) => payload.action === 'key_matrix' ? d.promise : { ok: true });
  h.api.openMatrix('A'); const work = h.api.mxRun({ keys: ['A-k1'], endpoints: [E1] });
  const p = h.requests()[0].payload;
  h.sites[0].keys[0].base_url = 'https://new.invalid/v1'; h.api.renderMatrix();
  assert.equal(h.api.MX.busy, false); assert.equal(h.api.MX.cells.size, 0);
  await Promise.resolve();
  assert.equal(h.calls.filter((c) => c.payload.action === 'key_matrix_cancel').length, 1);
  d.resolve(reply(p)); await work; assert.equal(h.api.MX.cells.size, 0);
});

test('取消IPC失败不锁住会话，旧结果仍不得回写', async () => {
  const d = deferred(); const h = harness([fixture()], (_cmd, { payload }) => payload.action === 'key_matrix' ? d.promise : Promise.reject(new Error('取消IPC失败')));
  h.api.openMatrix('A'); const work = h.api.mxRun({ keys: ['A-k1'], endpoints: [E1] });
  const p = h.requests()[0].payload; h.api.mxCancel();
  assert.equal(h.api.MX.busy, false);
  d.resolve(reply(p)); await work;
  await new Promise(setImmediate);
  assert.equal(h.cell().state, 'cancelled');
  assert.ok(h.toasts.some(([msg, ok]) => msg.includes('取消通知失败') && ok === false));
});

test('获取模型防重入、部分失败刷新，finally恢复按钮', async () => {
  const d = deferred(); const h = harness([fixture()], () => d.promise);
  const info = h.api.siteInfo(h.sites[0]);
  const button = descendants(info).find((n) => n.tagName === 'BUTTON' && n.textContent === '获取模型');
  const work = h.api.pullSiteModelsAll(h.sites[0], button);
  assert.equal(button.disabled, true); assert.ok(h.api.SITE_MODELS_BUSY.has('A'));
  await h.api.pullSiteModelsAll(h.sites[0], button); assert.equal(h.calls.length, 1);
  d.resolve({ ok: false, msg: '部分目录获取失败' }); await work;
  assert.equal(button.disabled, false); assert.equal(button.textContent, '获取模型');
  assert.equal(h.api.SITE_MODELS_BUSY.size, 0); assert.equal(h.loads, 1);
  assert.ok(h.toasts.some(([msg, ok]) => msg === '部分目录获取失败' && ok === false));
});

test('获取模型IPC异常有toast并释放busy，用户可以重试', async () => {
  const h = harness([fixture()], () => Promise.reject(new Error('目录IPC断开')));
  const info = h.api.siteInfo(h.sites[0]);
  const button = descendants(info).find((n) => n.tagName === 'BUTTON' && n.textContent === '获取模型');
  await button.dispatch('click');
  assert.equal(h.api.SITE_MODELS_BUSY.size, 0); assert.equal(button.disabled, false);
  assert.ok(h.toasts.some(([msg, ok]) => msg.includes('获取模型失败') && ok === false));
  await button.dispatch('click'); assert.equal(h.calls.length, 2);
});
