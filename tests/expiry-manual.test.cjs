/* 积分包编辑：界面这一层的三条规矩
   ① 每档都有铅笔（厂商给没给日期都一样），不再有「填到期日」这种行内按钮；
   ② 弹窗字段照「编辑密钥」列：名称 / 截止日期 / 备注；自己加的包才多总额与剩余，且能删；
   ③ 保存只发一条请求、取消不发；日期只回填到日（原生 date 弹日历那套）。 */
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const APP = path.join(__dirname, '../ui/app.js');

function source(name, endMarker) {
  const app = fs.readFileSync(APP, 'utf8');
  const start = app.indexOf(name);
  assert.ok(start > 0, `找不到 ${name}`);
  const end = app.indexOf(endMarker, start);
  assert.ok(end > start, `找不到 ${name} 的结尾`);
  return app.slice(start, end);
}

/* 弹窗实现只依赖 el/btn/invoke/toast/load/$ —— 这里给一套最小替身：
   $ 返回一个带 value/hidden 的假 input，点按钮就能看到它发什么请求。 */
function fixture({ fields = {}, answers = [true] } = {}) {
  const invokes = [];
  const toasts = [];
  const inputs = {};
  const make = (tag, attrs = {}, ...kids) => ({ tag, attrs, kids: kids.flat().filter((k) => k != null) });
  const input = (id) => {
    if (!inputs[id]) {
      inputs[id] = {
        id, value: fields[id] ?? '', hidden: false, focused: false, textContent: '',
        focus() { this.focused = true; },
      };
    }
    return inputs[id];
  };
  const ctx = {
    EXP_NONE: '未给出',
    el: make,
    btn: (label, cls, click, title) => Object.assign(make('button', { class: cls, title }, label), { click }),
    iconBtn: (act, click, title) => Object.assign(make('button', { act, title }), { click }),
    invoke: async (_cmd, arg) => { invokes.push(arg.payload); return { ok: true, msg: 'ok' }; },
    toast: (m, ok) => toasts.push([m, ok]),
    load: async () => {},
    askConfirm: async () => answers.shift() ?? true,
    S: {},
    $: input,
    numv: (v) => (typeof v === 'number' ? v : (v == null || v === '' ? null : Number(v))),
    renderQuotaDetail: () => {},
  };
  vm.createContext(ctx);
  vm.runInContext(source('function expiryHas(', '/* 账号设置里的「积分明细」整块'), ctx);
  // `let qpEditing` 是词法声明，不会变成 context 的属性——要用 runInContext 读它
  const peek = () => vm.runInContext('qpEditing', ctx);
  return { ctx, invokes, toasts, inputs, input, peek };
}

test('no inline fill-date button survives — every pack is edited through the modal', () => {
  const app = fs.readFileSync(APP, 'utf8');
  assert.ok(!app.includes('expiryEdit'), '旧的「填到期日」入口应该已经删掉');
  assert.ok(!app.includes("'填到期日'"), '界面上不该再出现「填到期日」这几个字');
  assert.match(app, /iconBtn\('edit', \(\) => openPackModal/, '每档该挂铅笔');
});

test('outer group with items only lets you rename it — the date field is hidden', async () => {
  const f = fixture();
  f.ctx.openPackModal({ app: 'qoder', name: '号1' },
    { name: '平台奖励积分', orig: '平台奖励积分', gid: 'g:平台奖励积分', expires: '2026-11-07 10:00',
      items: [{ label: '平台赠送' }, { label: '平台赠送' }] });
  // 有 32 笔的那种档：日期是逐笔算出来的汇总，不给手填
  assert.equal(f.input('qp-date').hidden, true, '有逐笔的档不该让她填整档日期');
  assert.equal(f.input('qp-date-row').hidden, true, '整行都要收掉，别留一个空着的日期格');
  assert.equal(f.input('qp-date-label').hidden, true, '标签也跟着收');
  assert.equal(f.input('qp-total').hidden, true, '厂商的档不该出现总额输入框');
  assert.equal(f.input('qp-del').hidden, true, '厂商的档不该有删除');
  assert.match(f.input('qp-hint').textContent, /查看明细/, '要指她去明细里改每一笔');
  f.input('qp-name').value = 'SOTA专属积分';
  await f.ctx.savePackModal();
  assert.equal(f.invokes[0].key, 'g:平台奖励积分');
  assert.equal(f.invokes[0].label, 'SOTA专属积分');
  assert.equal(f.invokes[0].date, '', '外层档不提交日期，免得把汇总顶掉');
});

test('a group that has no items at all still gets the date field', async () => {
  const f = fixture();
  f.ctx.openPackModal({ app: 'qoder', name: '号1' },
    { name: '其他', orig: '其他', gid: 'g:其他', expires: '未给出', items: [] });
  assert.equal(f.input('qp-date').hidden, false, '没有逐笔的档，它自己就是唯一那一笔');
  f.input('qp-date').value = '2026-11-08';
  await f.ctx.savePackModal();
  assert.equal(f.invokes[0].date, '2026-11-08');
});

test('an inner pack is edited by its own key, and the vendor date is kept as a fallback', async () => {
  const f = fixture();
  const group = { name: '平台奖励积分', orig: '平台奖励积分' };
  const it = { label: '平台赠送', orig: '平台赠送', pid: 'b2', iid: 'i:b2',
    expires_on: '2026-11-19 20:24', expires_vendor: '2026-11-19 20:24', detail: 'RESOURCE_PACKAGE_SOURCE_BONUS' };
  f.ctx.openItemModal({ app: 'qoder', name: '号1' }, group, it);
  assert.match(f.input('qp-title').textContent, /编辑积分包/);
  assert.equal(f.input('qp-date').hidden, false, '内层这一笔必须能改截止日期');
  assert.equal(f.input('qp-date-row').hidden, false, '整行要显形');
  assert.match(f.input('qp-hint').textContent, /2026-11-19 20:24/, '要说清厂商原给的是哪天');
  f.input('qp-name').value = 'SOTA 赠送 A';
  f.input('qp-date').value = '2026-11-30';
  await f.ctx.savePackModal();
  assert.equal(f.invokes[0].action, 'quota_pack_save');
  assert.equal(f.invokes[0].key, 'i:b2', '按 pid 认人，不能按名字——32 笔全叫「平台赠送」');
  assert.equal(f.invokes[0].label, 'SOTA 赠送 A');
  assert.equal(f.invokes[0].date, '2026-11-30');
});

test('two packs with the same name get different keys', () => {
  const f = fixture();
  const group = { name: '平台奖励积分', orig: '平台奖励积分' };
  f.ctx.openItemModal({ app: 'qoder', name: '号1' }, group, { label: '平台赠送', pid: 'b2' });
  const a = f.peek().key;
  f.ctx.openItemModal({ app: 'qoder', name: '号1' }, group, { label: '平台赠送', pid: 'b3' });
  const b = f.peek().key;
  assert.notEqual(a, b, '同名不同笔必须写回不同的键，否则改一笔会改到别人头上');
});

test('custom pack exposes total/left and can be deleted', async () => {
  const pack = { name: '手工包', custom: true, custom_id: 'c1', gid: 'c:c1', total: 2000, left: 2000 };
  const f = fixture();
  f.ctx.openPackModal({ app: 'qoder', name: '号1' }, pack);
  assert.equal(f.input('qp-total').hidden, false);
  assert.equal(f.input('qp-del').hidden, false);
  f.input('qp-date').value = '2026-12-31';
  await f.ctx.savePackModal();
  assert.equal(f.invokes[0].action, 'quota_pack_custom');
  assert.equal(f.invokes[0].id, 'c1');
  assert.equal(f.invokes[0].total, '2000');
  // 存完弹窗关掉；再打开同一个包才谈得上删
  f.ctx.openPackModal({ app: 'qoder', name: '号1' }, pack);
  await f.ctx.deletePackModal();
  assert.equal(f.invokes.at(-1).action, 'quota_pack_custom');
  assert.equal(f.invokes.at(-1).delete, true);
});

test('adding a brand-new pack sends an empty id so the backend mints one', async () => {
  const f = fixture();
  f.ctx.openPackModal({ app: 'qoder', name: '号1' }, { custom: true, custom_id: '', name: '', gid: 'c:' });
  f.input('qp-name').value = '新包';
  await f.ctx.savePackModal();
  assert.equal(f.invokes[0].action, 'quota_pack_custom');
  assert.equal(f.invokes[0].id, '', '新增时 id 要空着，交给后端生成');
  assert.equal(f.invokes[0].label, '新包');
});

test('an empty name on a custom pack is refused before any request', async () => {
  const f = fixture();
  f.ctx.openPackModal({ app: 'qoder', name: '号1' }, { custom: true, custom_id: '', name: '' });
  f.input('qp-name').value = '   ';
  await f.ctx.savePackModal();
  assert.equal(f.invokes.length, 0);
  assert.equal(f.toasts.at(-1)[1], false);
});

test('clearing all three fields on a vendor pack restores the original name', async () => {
  const f = fixture();
  f.ctx.openPackModal({ app: 'qoder', name: '号1' }, { name: 'SOTA专属积分', orig: '其他', gid: 'g:其他' });
  // 三项都清空 = 恢复成厂商原样（后端 upsert 会删掉这条覆盖）
  f.input('qp-name').value = '';
  f.input('qp-date').value = '';
  f.input('qp-note').value = '';
  await f.ctx.savePackModal();
  assert.equal(f.invokes[0].action, 'quota_pack_save');
  assert.equal(f.invokes[0].label, '', '三项全空 = 交给后端删掉这条覆盖');
});

test('date field accepts day or minute, and the picker keeps the time she chose', () => {
  const f = fixture();
  assert.equal(f.ctx.qpDateOnly('2026-12-31 23:59'), '2026-12-31');
  assert.equal(f.ctx.qpDateOnly(''), '');
  assert.equal(f.ctx.expiryHas('未给出'), false);
  assert.equal(f.ctx.expiryHas('2026-12-31'), true);
  // 原生控件给的是 `YYYY-MM-DDTHH:MM`：只挑日期（00:00）写成纯日期，
  // 挑了具体时刻就原样带上——后端 norm_date 两种都收
  const native = { id: 'qp-native', value: '', addEventListener: (ev, fn) => { native.on = fn; } };
  const text = { id: 'qp-date', value: '', addEventListener: (ev, fn) => { text.on = fn; } };
  const pick = { id: 'qp-date-pick', addEventListener: (ev, fn) => { pick.on = fn; } };
  const saved = f.ctx.$;
  f.ctx.$ = (id) => ({ 'qp-native': native, 'qp-date': text, 'qp-date-pick': pick }[id] || saved(id));
  f.ctx.bindPackDatePicker();
  native.value = '2026-12-31T00:00';
  native.on();
  assert.equal(text.value, '2026-12-31', '只挑日期就写纯日期');
  native.value = '2026-12-31T22:22';
  native.on();
  assert.equal(text.value, '2026-12-31 22:22', '挑了时刻就带上时刻');
  f.ctx.$ = saved;
});

test('the pack editor stacks above the detail sheet it is opened from', () => {
  const css = fs.readFileSync(path.join(__dirname, '../ui/styles.css'), 'utf8');
  // 只认 `#id { ... z-index: N ... }` 这一种写法；查不到就报错，而不是默默放过
  const z = (id) => {
    const m = css.match(new RegExp('#' + id + '\\s*\\{[^}]*z-index:\\s*(\\d+)'));
    return m ? Number(m[1]) : null;
  };
  const detail = z('qmodal');
  const editor = z('qpmodal');
  assert.ok(detail != null && editor != null, '两个弹层都要显式给层级');
  assert.ok(editor > detail,
    `编辑弹窗(${editor}) 必须盖在明细(${detail}) 上面——它就是从那层点出来的`);
});

test('the native date picker is hidden for both modals', () => {
  const css = fs.readFileSync(path.join(__dirname, '../ui/styles.css'), 'utf8');
  const rule = css.match(/#k-exp-native, #qp-native {[^}]*opacity: 0[^}]*}/);
  assert.ok(rule, '两个原生日期框都要收起来，否则会在弹窗里露出来');
});
