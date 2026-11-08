const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

function fixture(chart = null) {
  const app = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');
  const source = app.match(/function usageTrendPanel\(instance\) \{[\s\S]*?\n\}/);
  assert.ok(source, 'the common usage component must exist');
  const calls = [];
  const context = {
    S: { detail: { dirty: false }, view: 'agents' },
    el: (tag, attrs, ...children) => ({ tag, attrs, children: children.flat().filter(x => x != null) }),
    btn: (label, cls, click) => ({ tag: 'button', label, click }),
    trendCharts: q => { calls.push(['trend', q]); return chart; },
    closeDetail: async () => { context.S.detail = null; },
    render: () => calls.push(['render']),
    renderDetail: () => calls.push(['renderDetail']),
    UsageDashboard: { selectApp: app => calls.push(['select', app]) },
  };
  vm.createContext(context);
  vm.runInContext(source[0], context);
  return { context, calls };
}
const text = node => typeof node === 'string' ? node : (node.children || []).map(text).join('');

test('unavailable data is explained without inventing a graph', async () => {
  const { context, calls } = fixture();
  const panel = context.usageTrendPanel({ app: 'catpaw', quota: {} });
  assert.match(text(panel), /尚未找到可读取/);
  assert.ok(!text(panel).includes('0 Token'));
  const button = panel.children.find(x => x.tag === 'div').children.find(x => x.tag === 'button');
  await button.click();
  assert.equal(context.S.view, 'usage');
  assert.deepEqual(calls.at(-1), ['select', 'catpaw']);
});

test('a source-specific failure reason is visible', () => {
  const { context } = fixture();
  const panel = context.usageTrendPanel({ app: 'example', quota: { trend_reason: '当前目录不可读' } });
  assert.match(text(panel), /当前目录不可读/);
});

test('an existing trend is rendered and passed through unchanged', () => {
  const chart = { tag: 'svg', children: [] };
  const { context, calls } = fixture(chart);
  const quota = { trend: { unit: 'tokens', days: [{ date: '2026-10-01', credits: 100 }] } };
  const panel = context.usageTrendPanel({ app: 'opencode', quota });
  assert.equal(calls[0][1], quota);
  assert.ok(panel.children.includes(chart));
  assert.doesNotMatch(text(panel), /暂无法展示/);
});

test('independent local tokens and saved credits can be selected without mixing them', () => {
  const chart = { tag: 'svg', children: [] };
  const { context, calls } = fixture(chart);
  const local = { unit: 'tokens', days: [{ date: '2026-10-01', credits: 100 }], scope: 'device' };
  const vendor = { unit: 'credits', days: [{ date: '2026-10-01', credits: 8 }], source: '官网' };
  const instance = { app: 'qoder-cn', quota: { local_trend: local, vendor_trend: vendor, trend: local } };
  const panel = context.usageTrendPanel(instance);
  assert.equal(calls[0][1].trend, local);
  assert.match(text(panel), /共用历史无法按账号准确拆分/);
  const switches = panel.children.find(n => n.attrs?.class?.includes('trend-source-switch'));
  assert.ok(switches, 'two distinct histories expose a source switch');
  const vendorButton = switches.children.find(n => n.label === '额度历史');
  vendorButton.click();
  assert.equal(context.S.detail.trendSource, 'vendor');
  assert.equal(context.S.trendSel, '');
  assert.equal(calls.at(-1)[0], 'renderDetail');
  context.usageTrendPanel(instance);
  assert.equal(calls.at(-1)[1].trend, vendor);
});

test('a source read failure is visible even when vendor history is available', () => {
  const { context } = fixture({ tag: 'svg', children: [] });
  const panel = context.usageTrendPanel({ app: 'qoder', quota: {
    local_trend_error: '采集程序启动失败',
    vendor_trend: { unit: 'credits', days: [{ date: '2026-10-01', credits: 8 }] },
  }});
  assert.match(text(panel), /本地记录读取失败：采集程序启动失败/);
  assert.doesNotMatch(text(panel), /暂无法展示用量趋势/);
});

test('identical local and saved histories do not create a misleading duplicate switch', () => {
  const { context } = fixture({ tag: 'svg', children: [] });
  const trend = { unit: 'tokens', days: [{ date: '2026-10-01', credits: 100 }] };
  const panel = context.usageTrendPanel({ app: 'codex', quota: { local_trend: trend, vendor_trend: trend } });
  assert.equal(panel.children.some(n => n.attrs?.class?.includes('trend-source-switch')), false);
});

test('special account detail paths use the common component', () => {
  const read = name => fs.readFileSync(path.join(__dirname, '../ui', name), 'utf8');
  assert.match(read('chatgpt.js'), /secPanel\('额度与用量', chatgptQuotaPanel\(instance\), usageTrendPanel\(instance\)\)/);
  assert.match(read('antigravity.js'), /quotaPanel\(instance\)/);
  assert.match(read('zcode.js'), /options\.compact \? null : usageTrendPanel\(instance\)/);
  assert.match(read('app.js'), /wrap\.appendChild\(usageTrendPanel\(i\)\)/);
});
