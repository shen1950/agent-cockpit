const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
class Element {
  constructor(tag, attrs = {}, children = []) { this.tag = tag; Object.assign(this, attrs); if (attrs.aria) for (const [k,v] of Object.entries(attrs.aria)) this[`aria-${k}`]=v; this.children = children.flat().filter(x => x != null); this.events = {}; this.value ??= ''; }
  append(...children) { this.children.push(...children.filter(x => x != null)); }
  appendChild(child) { this.append(child); return child; }
  replaceChildren(...children) { this.children = children.flat().filter(x => x != null); }
  set textContent(value) { this.children = [String(value)]; }
  get textContent() { return text(this); }
  setAttribute(key, value) { this[key] = value; }
  addEventListener(key, fn) { this.events[key] = fn; }
  focus() { this.focused = true; }
  select() { this.selected = true; }
  remove() { this.removed = true; }
}
function descendants(node) { return [node, ...(node.children || []).flatMap(x => typeof x === 'object' ? descendants(x) : [])]; }
function text(node) { return (node.children || []).map(x => typeof x === 'object' ? text(x) : String(x)).join(''); }
function setup(tab = 'client', native = false, options = {}) {
  const requests = [], notices = [];
  const timers = new Map(); let nextTimer = 0;
  const dom = new Map(['im-save', 'im-nav', 'im-body'].map(id => [id, new Element('div', { id })]));
  const context = { console, requests, notices, S: { detail: { app: 'codex-desktop', name: 'sample', sec: 'auth', chatgptTab: tab, f: {} }, data: { apps: [{ id: 'codex-desktop' }], instances: [] } },
    el: (tag, attrs, ...children) => new Element(tag, attrs, children),
    btn: (label, style, action) => Object.assign(new Element('button', { style }, [label]), { action }),
    row: (...args) => new Element('row', {}, args), secPanel: (...args) => new Element('section', {}, args),
    qhelp: hint => new Element('span', { class: 'qhelp', role: 'note', hint, 'aria-label': `说明：${hint}` }, ['?']),
    setTimeout: (fn) => { const id = ++nextTimer; timers.set(id, fn); return id; }, clearTimeout: (id) => timers.delete(id),
    document: { body: new Element('body') }, navigator: { clipboard: { writeText: async () => {} } },
    $: id => dom.get(id), clear: node => node.replaceChildren(),
    toast: (...args) => notices.push(args), render: () => {}, renderDetail: () => {}, openDetail: async (...args) => requests.push(['openDetail', ...args]),
    askConfirm: async (...args) => options.confirm ? options.confirm(...args) : true,
    invoke: async (method, args) => { requests.push([method, args]); return method === 'get_state' ? { apps: [{ id: 'codex-desktop' }] } : options.respond ? options.respond(args.payload) : { ok: true, name: 'sample' }; },
    run: async (action, payload) => { requests.push([action, payload]); return { ok: true }; }
  };
  vm.createContext(context);
  vm.runInContext(fs.readFileSync(path.join(__dirname, '../ui/chatgpt.js'), 'utf8'), context);
  const instance = { app: 'codex-desktop', name: 'sample', in_place: native, chatgpt_auth: { native, ...options.auth } };
  const genericFields = context.S.detail.f;
  const panel = options.add ? context.openChatgptAdd({ id: 'codex-desktop' }) : context.chatgptAuthPanel('codex-desktop', 'sample', instance);
  return { context, panel, requests, notices, instance, genericFields, dom, nodes: descendants(panel), timers,
    tick: async (functionName) => { const entry = [...timers].find(([, fn]) => fn.name === functionName); assert.ok(entry, `missing timer ${functionName}`); timers.delete(entry[0]); await entry[1](); } };
}
function button(panel, label) { return descendants(panel).find(n => n.tag === 'button' && text(n) === label); }
function actionPayloads(s, action) { return s.requests.filter(r => r[0] === 'action' && r[1].payload.action === action).map(r => r[1].payload); }
test('all upstream login entry types are reachable', () => {
  const s = setup();
  for (const title of ['官方登录', 'OAuth', 'Token / JSON', 'API Key', '导入']) assert.ok(s.nodes.some(n => n.tag === 'button' && text(n) === title));
  assert.ok(s.nodes.some(n => text(n) === '开始官方登录'));
});
test('API credentials stay out of generic metadata and are cleared after submission', async () => {
  const s = setup('apikey');
  const key = s.nodes.find(n => n.type === 'password'); key.value = 'sk-synthetic'; key.events.input();
  const base = s.nodes.find(n => n.type === 'url'); base.value = 'http://127.0.0.1:9999/v1'; base.events.input();
  await s.nodes.find(n => n.tag === 'button' && text(n) === '保存 API Key').action();
  const sent = s.requests.find(r => r[0] === 'chatgpt_apikey')[1];
  assert.equal(sent.key, 'sk-synthetic'); assert.equal(sent.base_url, base.value);
  assert.equal(key.value, ''); assert.deepEqual(Object.keys(s.context.S.detail.f), []);
});
test('native account never offers credential replacement', () => {
  const s = setup('token', true);
  assert.equal(s.nodes.some(n => n.tag === 'textarea'), false);
  assert.equal(s.nodes.some(n => n.tag === 'button' && text(n) === '导入这份凭据'), false);
  assert.ok(s.nodes.some(n => n.tag === 'button' && text(n) === '新建独立账号'));
});
test('device authorization and cancellation carry only the selected account', async () => {
  const s = setup('oauth');
  await s.nodes.find(n => n.tag === 'button' && text(n) === '设备码登录').action();
  await s.nodes.find(n => n.tag === 'button' && text(n) === '取消本次授权').action();
  assert.equal(s.requests[0][0], 'chatgpt_login'); assert.equal(s.requests[0][1].method, 'device');
  assert.equal(s.requests[0][1].name, 'sample'); assert.equal(s.requests[1][0], 'chatgpt_cancel');
});
test('oversized credential files are rejected before reading', async () => {
  const s = setup('token'); const file = s.nodes.find(n => n.type === 'file'); let read = false;
  file.files = [{ size: 9 * 1024 * 1024, text: async () => { read = true; return ''; } }];
  await file.events.change(); assert.equal(read, false); assert.ok(s.notices.length);
});

test('batch import names new accounts explicitly and keeps secrets out of metadata', async () => {
  const s = setup('token');
  const prefix = s.nodes.find(n => n['aria-label'] === '批量账号名前缀'); prefix.value = 'work'; prefix.events.input();
  const input = s.nodes.find(n => n.tag === 'textarea'); input.value = '[{"access_token":"synthetic"}]'; input.events.input();
  await s.nodes.find(n => n.tag === 'button' && text(n) === '批量创建独立账号').action();
  const payload = s.requests.find(r => r[0] === 'chatgpt_import_many')[1];
  assert.equal(payload.prefix, 'work'); assert.equal(payload.text, input.value);
  assert.deepEqual(Object.keys(s.context.S.detail.f), []);
});

test('add dialog starts with five login methods, optional name and one shared history', () => {
  const s = setup('client', false, { add: true });
  for (const label of ['官方登录', 'OAuth', 'Token / JSON', 'API Key', '导入']) assert.ok(button(s.panel, label));
  assert.equal(s.nodes.find(n => n['aria-label'] === '账号名称（可选）').value, '');
  assert.equal(s.nodes.find(n => n['aria-label'] === '新账号的对话和项目').value, 'shared');
  const historyHelp = s.nodes.find(n => n.class === 'qhelp' && n.hint.includes('共用一份本机对话'));
  assert.ok(historyHelp);
  assert.match(historyHelp.hint, /共用一份本机对话和项目/);
  assert.match(historyHelp.hint, /聊天正文不重复保存/);
  assert.doesNotMatch(text(s.panel), /新增对话暂不实时同步/);
  assert.equal(s.requests.length, 0);
  const panel = s.nodes.find(n => n.role === 'tabpanel');
  const selected = s.nodes.find(n => n.role === 'tab' && n['aria-selected'] === 'true');
  assert.equal(panel['aria-labelledby'], selected.id); assert.equal(selected['aria-controls'], panel.id);
});

test('official login shows all six steps and creates no blank card before completion', async () => {
  let phase = 'waiting';
  const s = setup('client', false, { add: true, respond: (p) => ({ ok: true, session: { id: 'session-1', phase, running: phase !== 'completed', name: 'auto-name', message: '等待登录' } }) });
  for (const label of ['准备临时配置', '打开官方客户端', '等待登录完成', '关闭官方客户端', '读取登录信息', '清理临时配置']) assert.match(text(s.panel), new RegExp(label));
  await button(s.panel, '开始官方登录').action();
  const start = actionPayloads(s, 'chatgpt_official_start')[0];
  assert.equal(start.name, ''); assert.equal(start.new_name, ''); assert.equal(start.history_mode, 'shared');
  assert.equal(actionPayloads(s, 'chatgpt_create').length, 0);
  assert.equal(s.requests.some(r => r[0] === 'get_state'), false);
  phase = 'completed'; await s.tick('poll');
  assert.equal(s.panel.removed, true);
  assert.ok(s.requests.some(r => r[0] === 'get_state'));
  assert.equal(s.requests.some(r => r[0] === 'openDetail'), false);
  assert.equal(s.context.S.cur, 'codex-desktop'); assert.equal(s.context.S.view, 'agents');
});

test('official close requires cancellation and preserves the open dialog when declined', async () => {
  let allowed = false;
  const s = setup('client', false, { add: true, confirm: () => allowed,
    respond: (p) => ({ ok: true, session: { id: 'cancel-session', phase: p.action === 'chatgpt_official_cancel' ? 'cancelled' : 'waiting', running: p.action !== 'chatgpt_official_cancel' } }) });
  await button(s.panel, '开始官方登录').action();
  await button(s.panel, '关闭').action();
  assert.notEqual(s.panel.removed, true); assert.equal(actionPayloads(s, 'chatgpt_official_cancel').length, 0);
  allowed = true; await button(s.panel, '关闭').action();
  assert.equal(actionPayloads(s, 'chatgpt_official_cancel')[0].session_id, 'cancel-session');
  assert.equal(s.panel.removed, true);
});

test('failed official import can retry the same retained session without creating a card', async () => {
  const s = setup('client', false, { add: true, respond: (p) => ({ ok: true,
    session: { id: 'retained-session', phase: p.action === 'chatgpt_official_retry' ? 'completed' : 'failed', running: false, can_retry: true, name: 'recovered' } }) });
  await button(s.panel, '开始官方登录').action();
  assert.equal(button(s.panel, '重试本次登录').hidden, false);
  assert.equal(s.requests.some(r => r[0] === 'get_state'), false);
  await button(s.panel, '重试本次登录').action();
  assert.equal(actionPayloads(s, 'chatgpt_official_retry')[0].session_id, 'retained-session');
  assert.equal(s.panel.removed, true);
});

test('official start error remains visible after busy state clears', async () => {
  const s = setup('client', false, { add: true, respond: () => ({ ok: false, msg: '临时客户端未能启动' }) });
  await button(s.panel, '开始官方登录').action();
  assert.match(text(s.panel), /临时客户端未能启动/);
  assert.equal(button(s.panel, '开始官方登录').disabled, false);
});

test('failed official login can clear its session and unlock optional account settings', async () => {
  const s = setup('client', false, { add: true, respond: (p) => ({ ok: true,
    session: { id: 'clear-failed', phase: p.action === 'chatgpt_official_cancel' ? 'cancelled' : 'failed', running: false, can_retry: true, imported: false } }) });
  await button(s.panel, '开始官方登录').action();
  const name = descendants(s.panel).find(n => n['aria-label'] === '账号名称（可选）');
  const history = descendants(s.panel).find(n => n['aria-label'] === '新账号的对话和项目');
  assert.equal(name.disabled, true); assert.equal(history.disabled, true);
  assert.equal(button(s.panel, '清理临时配置并重新开始').hidden, false);
  await button(s.panel, '清理临时配置并重新开始').action();
  assert.equal(name.disabled, false); assert.equal(history.disabled, false);
  assert.equal(button(s.panel, '开始官方登录').hidden, false);
});

test('an imported official login whose cleanup failed only offers cleanup retry', async () => {
  const s = setup('client', false, { add: true, respond: () => ({ ok: true,
    session: { id: 'cleanup-failed', phase: 'failed', running: false, can_retry: true, imported: true } }) });
  await button(s.panel, '开始官方登录').action();
  assert.equal(button(s.panel, '重试清理临时配置').hidden, false);
  assert.equal(button(s.panel, '清理临时配置并重新开始').hidden, true);
});

test('new API account submits directly without a preliminary empty account and clears secret fields', async () => {
  const s = setup('client', false, { add: true });
  await button(s.panel, 'API Key').action();
  const nodes = descendants(s.panel), key = nodes.find(n => n.type === 'password'), base = nodes.find(n => n.type === 'url');
  key.value = 'synthetic-key'; key.events.input(); base.value = 'https://api.example.invalid/v1'; base.events.input();
  await button(s.panel, '保存并添加账号').action();
  const p = actionPayloads(s, 'chatgpt_add_apikey')[0];
  assert.equal(p.key, 'synthetic-key'); assert.equal(p.new_name, ''); assert.equal(p.history_mode, 'shared');
  assert.equal(actionPayloads(s, 'chatgpt_create').length, 0);
  assert.equal(key.value, ''); assert.deepEqual(Object.keys(s.genericFields), []);
});

test('add dialog retains token input across method switches only in dialog memory', async () => {
  const s = setup('client', false, { add: true });
  await button(s.panel, 'Token / JSON').action();
  let input = descendants(s.panel).find(n => n.tag === 'textarea'); input.value = 'synthetic-token'; input.events.input();
  await button(s.panel, 'API Key').action(); await button(s.panel, 'Token / JSON').action();
  input = descendants(s.panel).find(n => n.tag === 'textarea'); assert.equal(input.value, 'synthetic-token');
  assert.deepEqual(Object.keys(s.context.S.detail.f), []);
  await button(s.panel, '关闭').action(); assert.equal(input.value, '');
});

test('native official login offers only a new independent account', async () => {
  const s = setup('client', true);
  assert.equal(button(s.panel, '开始官方登录'), undefined);
  await button(s.panel, '添加独立账号').action();
  const overlay = s.context.document.body.children.at(-1);
  assert.ok(button(overlay, '官方登录')); assert.equal(actionPayloads(s, 'chatgpt_official_start').length, 0);
});

test('an existing independent card uses the same temporary official flow for that account', async () => {
  const s = setup('client', false, { respond: () => ({ ok: true, session: { id: 'existing-session', phase: 'waiting', running: true } }) });
  await button(s.panel, '开始官方登录').action();
  const overlay = s.context.document.body.children.at(-1);
  await button(overlay, '开始官方登录').action();
  assert.equal(actionPayloads(s, 'chatgpt_official_start')[0].name, 'sample');
  assert.equal(actionPayloads(s, 'chatgpt_create').length, 0);
});

test('new token import uses the shared history and submits only on import', async () => {
  const s = setup('client', false, { add: true });
  await button(s.panel, 'Token / JSON').action();
  const nodes = descendants(s.panel), history = nodes.find(n => n['aria-label'] === '新账号的对话和项目'); assert.equal(history.value, 'shared');
  const input = nodes.find(n => n.tag === 'textarea'); input.value = '{"access_token":"synthetic"}'; input.events.input();
  assert.equal(actionPayloads(s, 'chatgpt_add_import').length, 0);
  await button(s.panel, '导入并添加账号').action();
  assert.equal(actionPayloads(s, 'chatgpt_add_import')[0].history_mode, 'shared');
  assert.equal(actionPayloads(s, 'chatgpt_add_import')[0].text, '{"access_token":"synthetic"}');
  assert.equal(input.value, '');
});

test('OAuth creates and starts once, then polls the returned account and cancels explicitly on close', async () => {
  const s = setup('client', false, { add: true, respond: (p) => p.action === 'chatgpt_status'
    ? { ok: true, status: { has_auth: false, login: { running: true, url: 'https://example.invalid/authorize' } } } : { ok: true, name: 'oauth-new' } });
  await button(s.panel, 'OAuth').action(); await button(s.panel, '设备码登录').action();
  assert.equal(actionPayloads(s, 'chatgpt_add_oauth')[0].method, 'device');
  assert.equal(actionPayloads(s, 'chatgpt_login').length, 0);
  assert.equal(actionPayloads(s, 'chatgpt_status')[0].name, 'oauth-new');
  await button(s.panel, '关闭').action();
  assert.equal(actionPayloads(s, 'chatgpt_cancel')[0].name, 'oauth-new'); assert.equal(s.panel.removed, true);
});

test('a saved API login opens its current identity instead of another login form', async () => {
  const s = setup('client', false, { auth: { has_auth: true, mode: 'apikey', base_url: 'https://relay.example/v1' } });
  assert.match(text(s.panel), /https:\/\/relay\.example\/v1/);
  assert.equal(descendants(s.panel).some(n => n.type === 'password'), false);
  assert.equal(button(s.panel, '开始官方登录'), undefined);
  await button(s.panel, '更换 API Key').action();
  const edit = s.context.chatgptAuthPanel('codex-desktop', 'sample', s.instance);
  assert.ok(descendants(edit).some(n => n.type === 'password'));
  assert.equal(s.context.S.detail.chatgptTab, 'apikey');
});

test('ChatGPT detail navigation follows use order and does not expose a generic save action', async () => {
  const s = setup(); s.context.S.detail.sec = 'info';
  s.context.renderDetail = () => s.context.renderChatgptDetail(s.context.S.detail, s.instance);
  s.context.renderDetail();
  const nav = s.dom.get('im-nav');
  assert.deepEqual(descendants(nav).filter(n => n.tag === 'button').map(text),
    ['概览', '额度与用量', '运行管理', '对话与项目', '文件位置', '登录管理', '移除账号']);
  assert.equal(s.dom.get('im-save').hidden, true);
  await button(nav, '文件位置').action();
  assert.equal(s.context.S.detail.sec, 'data');
  assert.match(text(s.dom.get('im-body')), /尚未记录这个目录的位置/);
  assert.equal(descendants(s.dom.get('im-body')).some(n => n.class === 'setrow'), false);
});

test('quota windows show remaining values and a real zero API balance is retained', () => {
  const s = setup();
  const plus = s.context.chatgptQuotaPanel({ app: 'codex-desktop', name: 'plus', chatgpt_auth: { mode: 'chatgpt' },
    chatgpt_quota: { state: 'live', fetched_at: 1700000000, windows: [{ id: '5h', used_percent: 25 }, { id: 'weekly', remaining_percent: 100 }] } });
  assert.deepEqual(descendants(plus).filter(n => n.tag === 'progress').map(n => n.value), [75, 100]);
  assert.match(text(plus), /5 小时限额/); assert.match(text(plus), /每周限额/);
  const api = s.context.chatgptQuotaPanel({ app: 'codex-desktop', name: 'api', chatgpt_auth: { mode: 'apikey' },
    chatgpt_quota: { state: 'live', fetched_at: 1700000000, balance: { remaining: 0, used: 12, currency: 'USD' } } });
  assert.match(text(api), /可用余额0 USD/); assert.match(text(api), /已用 12 USD/);
  assert.doesNotMatch(text(api), /旧缓存|读不到服务端/);
});

test('a first quota failure is not described as cached data', () => {
  const s = setup();
  const panel = s.context.chatgptQuotaPanel({ app: 'codex-desktop', name: 'api', chatgpt_auth: { mode: 'apikey' },
    chatgpt_quota: { state: 'error', message: '该接口返回 401' } });
  assert.match(text(panel), /该接口返回 401/);
  assert.doesNotMatch(text(panel), /旧缓存|上次成功/);
  assert.equal(descendants(panel).some(n => n.class === 'cpt-quota-message warning'), false);
});

test('a failed quota refresh keeps known readings and updates the latest state instance', async () => {
  let resolve;
  const s = setup('client', false, { respond: () => new Promise(r => { resolve = r; }) });
  const before = { app: 'codex-desktop', name: 'sample', chatgpt_quota: { state: 'live', balance: { remaining: 4.5, currency: 'USD' } } };
  s.context.S.data.instances = [before];
  const request = s.context.chatgptRefreshQuota('codex-desktop', 'sample');
  const current = { ...before }; s.context.S.data.instances = [current];
  resolve({ ok: false, msg: '暂时无法连接' }); await request;
  assert.equal(current.chatgpt_quota.state, 'cached');
  assert.equal(current.chatgpt_quota.balance.remaining, 4.5);
  const sent = actionPayloads(s, 'chatgpt_quota_refresh')[0];
  assert.deepEqual(Object.keys(sent).sort(), ['action', 'app', 'name']);
});

test('file locations separate labels, long paths and actions without blank values', () => {
  const s = setup();
  const path = 'C:\\Users\\Example\\.agent-cockpit\\chatgpt-auth\\account-with-a-long-fixed-directory-id';
  const panel = s.context.chatgptDataPanel('codex-desktop', 'sample', { data_dir: path + '\\browser', chatgpt_auth: { home: path } });
  const articles = descendants(panel).filter(n => n.tag === 'article'); assert.equal(articles.length, 2);
  assert.ok(articles.every(n => descendants(n).some(x => x.tag === 'h3') && descendants(n).some(x => x.tag === 'code')));
  assert.ok(descendants(panel).filter(n => n.tag === 'code').every(n => text(n).startsWith('C:\\Users\\Example')));
});

test('project repair reports pending work without ending a running window', async () => {
  const s = setup('client', false, { respond: p => ({ ok: true, msg: '已排队，下次启动应用。', project_repair: { pending: true } }) });
  const instance = { ...s.instance, running: true };
  const panel = s.context.chatgptSettingsPanel('codex-desktop', 'sample', instance, { sec: 'history' });
  await button(panel, '修正项目列表').action();
  assert.match(text(panel), /已排队/);
  assert.equal(button(panel, '已安排修正').disabled, true);
  assert.equal(actionPayloads(s, 'chatgpt_project_repair').length, 1);
  assert.equal(actionPayloads(s, 'kill').length, 0);
});

test('a resident ChatGPT account can reopen its own window without killing processes', async () => {
  const s = setup();
  const panel = s.context.chatgptSettingsPanel('codex-desktop', 'sample', { ...s.instance, resident: true }, { sec: 'run' });
  assert.ok(button(panel, '结束后台进程'));
  await button(panel, '打开窗口').action();
  assert.equal(s.requests.at(-1)[0], 'chatgpt_show_window');
  assert.equal(s.requests.at(-1)[1].name, 'sample');
  assert.equal(s.requests.some(r => r[0] === 'kill'), false);
});

test('a running ChatGPT account keeps Open separate from End in its overview', async () => {
  const s = setup();
  const panel = s.context.chatgptSettingsPanel('codex-desktop', 'sample', { ...s.instance, running: true }, { sec: 'info' });
  assert.ok(button(panel, '结束运行'));
  await button(panel, '打开窗口').action();
  assert.equal(s.requests.at(-1)[0], 'chatgpt_show_window');
  assert.equal(s.requests.at(-1)[1].app, 'codex-desktop');
});

test('run management separates opening from ending and has no redundant refresh action', async () => {
  const s = setup();
  const panel = s.context.chatgptSettingsPanel('codex-desktop', 'sample', { ...s.instance, running: true }, { sec: 'run' });
  const summary = descendants(panel).find(n => n.class === 'cpt-run-summary');
  const stop = descendants(panel).find(n => n.class === 'cpt-run-stop');
  assert.deepEqual(descendants(summary).filter(n => n.tag === 'button').map(text), ['打开窗口']);
  assert.deepEqual(descendants(stop).filter(n => n.tag === 'button').map(text), ['结束运行']);
  assert.equal(button(panel, '刷新状态'), undefined);
  assert.equal(descendants(panel).filter(n => n.tag === 'button').length, 2);
  assert.doesNotMatch(text(panel), /账号隔离|每个账号使用|窗口按 × 关闭后|请先保存/);
  assert.ok(descendants(stop).some(n => n.class === 'qhelp' && /仅结束此账号/.test(n.hint)));
  await button(stop, '结束运行').action();
  assert.equal(s.requests.at(-1)[0], 'kill');
  assert.equal(s.requests.at(-1)[1].name, 'sample');
});

test('a stopped account only offers startup in run management', async () => {
  const s = setup();
  const panel = s.context.chatgptSettingsPanel('codex-desktop', 'sample', s.instance, { sec: 'run' });
  assert.deepEqual(descendants(panel).filter(n => n.tag === 'button').map(text), ['启动']);
  assert.match(text(panel), /未运行/);
  await button(panel, '启动').action();
  assert.equal(s.requests.at(-1)[0], 'chatgpt_show_window');
  assert.equal(s.requests.at(-1)[1].name, 'sample');
});

test('file location help is optional while paths and actions remain visible', () => {
  const s = setup();
  const panel = s.context.chatgptDataPanel('codex-desktop', 'sample', {
    data_dir: 'C:\\managed\\browser', chatgpt_auth: { home: 'C:\\managed\\account' }
  });
  assert.match(text(panel), /C:\\managed\\browser/);
  assert.match(text(panel), /C:\\managed\\account/);
  assert.equal(descendants(panel).filter(n => n.tag === 'p').length, 0);
  assert.ok(descendants(panel).some(n => n.class === 'qhelp' && /移除账号登记后仍保留/.test(n.hint)));
});

test('quota removes routine explanations but retains reading time and cached failure', () => {
  const s = setup();
  const panel = s.context.chatgptQuotaPanel({ app: 'codex-desktop', name: 'sample', chatgpt_auth: { mode: 'chatgpt' },
    chatgpt_quota: { state: 'cached', fetched_at: 1700000000, message: '连接失败，显示上次读数。',
      windows: [{ id: '5h', remaining_percent: 35 }] } });
  assert.match(text(panel), /剩余 35%/);
  assert.match(text(panel), /更新于/);
  assert.match(text(panel), /连接失败，显示上次读数/);
  assert.doesNotMatch(text(panel), /百分比以服务返回|显示账号当前的使用窗口/);
  assert.equal(descendants(panel).some(n => n.class === 'cpt-note'), false);
});
