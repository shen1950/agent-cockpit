// 无浏览器回归：执行产品实际的总览、分类与逐包渲染函数，检查生成的 DOM。
// 通过 agsync build --test-cmd "node tests/catpaw-progress.test.cjs" 执行。
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const assert = require('node:assert/strict');
const source = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');
class Element {
  constructor(tag, attrs = {}, children = []) { this.tag = tag; this.attrs = attrs; this.children = []; this.append(...children); }
  append(...children) { this.children.push(...children.flat().filter(c => c != null)); }
  appendChild(child) { if (child != null) this.children.push(child); return child; }
  addEventListener() {}
  get textContent() { return this.children.map(c => typeof c === 'object' ? c.textContent : String(c)).join(''); }
  set textContent(value) { this.children = [value]; }
}
const el = (tag, attrs, ...children) => new Element(tag, attrs, children);
const ids = new Map();
const ctx = vm.createContext({
  console, UNIT_CN: { credits: '积分' }, EXP_NONE: '未给出', SCOPE_EXCLUSIVE: /专属/,
  S: { data: { instances: [] } }, qdetail: null,
  el, clear: e => { e.children = []; }, $: id => { if (!ids.has(id)) ids.set(id, el('div', {})); return ids.get(id); },
  btn: (label, cls) => el('button', { class: cls }, label), iconBtn: () => el('button', { class: 'iconbtn' }),
  usageTrendPanel: () => null, stampTime: () => '2026-11-18', dtxt: s => String(s),
});
for (const name of ['nfmt', 'utxt', 'numv', 'unitPlan', 'scopeTag', 'quotaHasData', 'quotaRows',
  'quotaGroups', 'quotaPct', 'usedPct', 'pctTxt', 'quotaAgg', 'quotaCaveat', 'expKey',
  'expCls', 'expText', 'expiryLabel', 'srcTxt', 'detailStamp', 'quotaPanel', 'renderQuotaDetail']) {
  const fn = source.match(new RegExp('^function ' + name + '\\([^]*?^}', 'm'));
  assert.ok(fn, `missing function ${name}`);
  vm.runInContext(fn[0], ctx);
}
const item = (label, total, used, left, date, expired = false, voided = null) => ({
  label, total, used, left, expires_on: date, expired, voided, used_est: true,
  unit: '积分', granted_on: '2026-07-28 00:49:27', granted_src: 'vendor',
});
const groups = [
  { name: '套餐基础积分', total: 200, used: 0, left: 200, live_count: 1, expired_count: 4, items: [
    item('赠送基础积分', 200, 200, 0, '2026-07-31 23:59:59', true),
    item('赠送基础积分', 200, 0, 200, '2026-08-31 23:59:59', true, 200),
    item('赠送基础积分', 200, 0, 200, '2026-09-30 23:59:59', true, 200),
    item('赠送基础积分', 200, 37.82, 162.18, '2026-10-31 23:59:59', true, 162.18),
    item('赠送基础积分', 200, 0, 200, '2026-11-30 23:59:59'),
  ] },
  { name: '平台奖励积分', total: 2000, used: 29.5, left: 1970.5, live_count: 2, expired_count: 0, items: [
    item('新用户注册奖励', 1000, 29.5, 970.5, '2027-07-28 23:59:59'),
    item('积分管理后台发放', 1000, 0, 1000, '2027-05-13 23:59:59'),
  ] },
];
const inst = { app: 'catpaw', name: '测试账号', quota: { left: 2170.5, total: 2200, groups } };
// 可选传入真实已保存的额度档案，只读，不复制凭据、不把姓名写入日志。
if (process.env.CATPAW_QUOTA_FIXTURE) {
  const saved = JSON.parse(fs.readFileSync(process.env.CATPAW_QUOTA_FIXTURE, 'utf8'));
  inst.quota = { left: saved.credits_left, total: saved.credits_total, groups: saved.quota_groups };
}
ctx.S.data.instances = [inst];
ctx.inst = inst;
function findClass(root, name) {
  if (!root || typeof root !== 'object') return [];
  return [...(String(root.attrs.class || '').split(' ').includes(name) ? [root] : []),
    ...root.children.flatMap(c => findClass(c, name))];
}
const panel = vm.runInContext('quotaPanel(inst)', ctx);
assert.equal(findClass(panel, 'bar').length, 1, '总览必须有进度条');
assert.equal(findClass(panel, 'cbar').length, 2, '两个分类各有进度槽');
assert.match(panel.textContent, /已用 29\.5 \/ 2,200 积分.*98\.66%/);
assert.match(panel.textContent, /套餐基础积分200 积分/);
assert.match(panel.textContent, /平台奖励积分1,970\.5 积分/);
const barWidths = [];
for (const [group, tab, expected] of [['套餐基础积分', 'live', 1], ['套餐基础积分', 'dead', 4], ['平台奖励积分', 'live', 2]]) {
  ctx.qdetail = { app: inst.app, name: inst.name, group, tab };
  vm.runInContext('renderQuotaDetail()', ctx);
  const list = ids.get('qm-list');
  const bars = findClass(list, 'cbar');
  assert.equal(bars.length, expected, `${group}/${tab} 每笔都有进度槽`);
  assert.match(list.textContent, /推算/);
  barWidths.push(...bars.map(b => b.children[0].attrs.style));
  if (tab === 'dead') {
    assert.match(list.textContent, /37\.82 \/ 200/);
    assert.match(list.textContent, /过期作废 162\.18/);
    assert.match(list.textContent, /已用 200 \/ 200/);
  }
}
assert.equal(barWidths.filter(w => w === 'width:0%').length, 4, '未使用的四笔保留空槽');
assert.ok(barWidths.includes('width:100%'), '首月包满格');
assert.ok(barWidths.some(w => Math.abs(parseFloat(w.slice(6)) - 18.91) < 0.001), '十月用量18.91%');
assert.ok(barWidths.some(w => Math.abs(parseFloat(w.slice(6)) - 2.95) < 0.001), '注册奖励用量2.95%');
console.log('PASS: 总览1条 + 分类2条 + 逐包7条；0/部分/全部使用及作废金额、推算标记、98.66%均正确。');
