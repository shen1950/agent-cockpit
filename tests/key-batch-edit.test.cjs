/* 密钥「批量编辑」：界面这一层的四条规矩（2026-11-11）
   ① 工具栏里除了「批量删除」还要有一个「批量编辑」入口（不用先勾才看得见）；
   ② 一次提交只带**勾上的那几项**，没勾的字段绝不进 patch；
   ③ 截止日期走同一套归一化，认不出来就当场拦住、不发请求；
   ④ 密钥原文 / id 永远不在可批量改的名单里（改一批原文成同一个是事故）。 */
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const ROOT = path.join(__dirname, '..');
const APP = fs.readFileSync(path.join(ROOT, 'ui/app.js'), 'utf8');
const HTML = fs.readFileSync(path.join(ROOT, 'ui/index.html'), 'utf8');
const KEYS_RS = fs.readFileSync(path.join(ROOT, 'src-tauri/src/keys.rs'), 'utf8');

function slice(startMarker, endMarker) {
  const s = APP.indexOf(startMarker);
  assert.ok(s > 0, `找不到 ${startMarker}`);
  const e = APP.indexOf(endMarker, s);
  assert.ok(e > s, `找不到 ${startMarker} 的结尾`);
  return APP.slice(s, e);
}

/* 只跑批量编辑那一小段：给它最小替身，行对象自己造，不用真 DOM */
function fixture(rows) {
  const invokes = [];
  const toasts = [];
  const nodes = {};
  const ctx = {
    bem: { sel: new Set(), groups: [] },
    $: (id) => (nodes[id] || (nodes[id] = { id, hidden: true, textContent: '', disabled: false })),
    toast: (m, ok) => toasts.push([m, ok]),
    invoke: async (_cmd, arg) => { invokes.push(arg.payload); return { ok: true, msg: '已改' }; },
    load: async () => {},
    renderKeys: () => {},
    S: { data: { keys: [] } },
    keySel: () => new Set(),
    document: { querySelectorAll: () => rows },
    normExpDate: (s) => {
      const t = String(s || '').trim();
      const m = t.match(/^(\d{4})[-/.年](\d{1,2})[-/.月](\d{1,2})日?$/);
      return m ? `${m[1]}-${m[2].padStart(2, '0')}-${m[3].padStart(2, '0')}` : t;
    },
    // renderBe / renderBeForm 不在这几个用例里跑，给空壳防止引用报错
    el: () => ({ appendChild() {}, childElementCount: 0 }),
    btn: () => ({ addEventListener() {}, disabled: false }),
    clear: () => {},
    chip: () => null,
    dtxt: (s) => s,
  };
  vm.createContext(ctx);
  vm.runInContext(slice('const bem = { sel:', 'async function bdDelete() {'), ctx);
  // bem 在脚本里是 const（词法全局），改它得从同一个上下文里改，不能靠 ctx.bem
  const pick = (sigs) => vm.runInContext(`bem.sel = new Set(${JSON.stringify(sigs)})`, ctx);
  return { ctx, invokes, toasts, pick };
}

function row(kind, checked, value) {
  return {
    querySelector: (sel) => (sel === '.be-on' ? { checked }
      : sel === '.be-val' ? { value, disabled: !checked } : null),
  };
}

test('工具栏里「批量编辑」和「批量删除」并排，不用先勾选就能看见', () => {
  assert.match(HTML, /id="btn-keys-edit"[^>]*>批量编辑</);
  assert.match(HTML, /id="btn-keys-batch"[^>]*>批量删除</);
  const editAt = HTML.indexOf('id="btn-keys-edit"');
  const batchAt = HTML.indexOf('id="btn-keys-batch"');
  assert.ok(editAt > 0 && batchAt > editAt, '批量编辑应排在批量删除前面');
  assert.match(HTML, /id="bemodal"/);
  assert.match(APP, /\$\('btn-keys-edit'\)\.addEventListener\('click', \(\) => openKeyEdit\(\)\)/);
});

test('只把勾上的项放进 patch，没勾的字段一个都不带', async () => {
  const rows = [
    row('text', true, '  Pot  '),      // label 勾了
    row('text', false, '官方申请'),     // source 没勾
    row('date', false, '2026-12-31'),
    row('text', false, '备注'),
    row('text', false, 'https://x'),
    row('bool', true, '0'),            // use_in_proxy 勾了、选「不进反代」
  ];
  const { ctx, invokes, pick } = fixture(rows);
  pick(['p1|k1', 'p1|k2']);
  await vm.runInContext('beApply()', ctx);
  assert.equal(invokes.length, 1);
  const p = invokes[0];
  assert.equal(p.action, 'key_batch_update');
  // vm 里造出来的对象是另一个 realm 的，deepStrictEqual 会因原型不同而误判，比 JSON
  assert.equal(JSON.stringify(p.ids), JSON.stringify([{ site: 'p1', id: 'k1' }, { site: 'p1', id: 'k2' }]));
  assert.equal(JSON.stringify(p.patch), JSON.stringify({ label: 'Pot', use_in_proxy: false }));
});

test('截止日期归一化后再发；认不出来就拦住、一条请求都不发', async () => {
  const okRows = [row('text', false, ''), row('text', false, ''), row('date', true, '2026/3/9')];
  const a = fixture(okRows);
  a.pick(['p1|k1']);
  await vm.runInContext('beApply()', a.ctx);
  assert.equal(JSON.stringify(a.invokes[0].patch), JSON.stringify({ expires_on: '2026-03-09' }));

  const badRows = [row('text', false, ''), row('text', false, ''), row('date', true, '下个月')];
  const b = fixture(badRows);
  b.pick(['p1|k1']);
  await vm.runInContext('beApply()', b.ctx);
  assert.equal(b.invokes.length, 0, '日期认不出时不该发请求');
  assert.equal(b.toasts.at(-1)[1], false);
});

test('可批量改的字段不含密钥原文与 id，且都在后端白名单里', () => {
  const { ctx } = fixture([]);
  const fields = vm.runInContext('BE_FIELDS.map((x) => x.f)', ctx);
  for (const bad of ['key', 'id', 'masked', 'injected']) {
    assert.equal(fields.includes(bad), false, `${bad} 不该出现在批量编辑里`);
  }
  // 后端 update_keys_batch 的 ALLOW 常量：界面能改的必须是它的子集，否则点了静默不生效
  const allow = KEYS_RS.match(/const ALLOW: &\[&str\] = &\[([\s\S]*?)\];/);
  assert.ok(allow, '找不到后端 ALLOW 白名单');
  const allowed = [...allow[1].matchAll(/"([a-z_]+)"/g)].map((m) => m[1]);
  for (const f of fields) assert.ok(allowed.includes(f), `${f} 不在后端白名单里`);
});
