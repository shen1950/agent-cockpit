/* 「模型反代」收进总页之后的两处硬要求：
   ① 实测次数：点「测试」要真的把这次结果计入逐模型实测（前端先本地 +1，再以后端真值为准）
   ② 每个 Agent 页不再有「模型反代」面板，只剩「接入本地网关」；标题字体与老的 panelhead 一致 */
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const app = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');

function extract(name) {
  const m = new RegExp(`(?:async )?function ${name}\\(`).exec(app);
  assert.ok(m, `app.js 里找不到 ${name}`);
  const start = m.index;
  const next = app.indexOf('\nfunction ', start + 1);
  return app.slice(start, next < 0 ? undefined : next);
}

function context(extra = {}) {
  const c = {
    S: { proxyPage: { models: null, modelsLoading: false, foldMap: null } },
    PX_FOLD_KEY: 'cockpit-proxy-fold',
    Map, JSON, localStorage: { getItem: () => null, setItem: () => {} },
    rerenderProxy() {},
    ...extra,
  };
  vm.createContext(c);
  return c;
}

test('实测质量行：没有计数说「暂无实测」，有计数给次数与成功率', () => {
  const c = context();
  vm.runInContext(extract('pxQualityLine'), c);
  assert.equal(c.pxQualityLine({ id: 'a/b' }).text, '暂无实测');
  assert.equal(c.pxQualityLine({ id: 'a/b', calls: 0 }).text, '实测 0 次 · 成功率 0%');
  const ok = c.pxQualityLine({ id: 'a/b', calls: 4, ok: 4 });
  assert.equal(ok.text, '实测 4 次 · 成功率 100%');
  assert.equal(ok.cls, 'ok');
  const bad = c.pxQualityLine({ id: 'a/b', calls: 4, ok: 1 });
  assert.equal(bad.text, '实测 4 次 · 成功率 25%');
  assert.equal(bad.cls, 'bad');
});

test('本地补记一次测试：calls/ok/fail 三个数要一起动', () => {
  const models = [{ id: 'astudio/spark-x2.6' }, { id: 'astudio/other' }];
  const c = context();
  c.S.proxyPage.models = { groups: [{ owner: 'astudio', models }] };
  vm.runInContext(extract('bumpModelStatLocal'), c);
  c.bumpModelStatLocal('astudio/spark-x2.6', true);
  assert.deepEqual([models[0].calls, models[0].ok, models[0].fail], [1, 1, 0]);
  c.bumpModelStatLocal('astudio/spark-x2.6', false);
  assert.deepEqual([models[0].calls, models[0].ok, models[0].fail], [2, 1, 1]);
  assert.equal(models[1].calls, undefined, '没测过的模型不许被写成 0');
});

test('点一次「测试」：走对 action、本地 +1、并回头重读后端真值', async () => {
  const seen = [];
  const fresh = { groups: [{ owner: 'astudio', models: [{ id: 'astudio/spark-x2.6', calls: 1, ok: 1 }] }] };
  const c = context({
    invoke: async (cmd, args) => {
      seen.push(args.payload);
      if (args.payload.action === 'proxy_models_list') return { msg: JSON.stringify(fresh) };
      return { ok: true, result: { ok: true, ms: 42 } };
    },
  });
  const model = { id: 'astudio/spark-x2.6' };
  c.S.proxyPage.models = { groups: [{ owner: 'astudio', models: [model] }] };
  for (const name of ['pxQualityLine', 'bumpModelStatLocal', 'refreshModelStats', 'pxTestModelCard']) {
    vm.runInContext(extract(name), c);
  }
  await c.pxTestModelCard(model, 'astudio');
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(seen[0].action, 'astudio_test');
  assert.equal(seen[0].model, 'spark-x2.6', '发给提供方的是裸模型名');
  assert.ok(seen.some((p) => p.action === 'proxy_models_list'), '测试完要重读一次全景，实测次数才会变');
  // 后端真值抵达后以它为准
  assert.equal(c.S.proxyPage.models.groups[0].models[0].calls, 1);
});

test('Agent 页不再有「模型反代」面板，只留「接入本地网关」且标题用 panelhead 字体', () => {
  const html = fs.readFileSync(path.join(__dirname, '../ui/index.html'), 'utf8');
  assert.doesNotMatch(html, /id="agent-proxy-panel"/);
  assert.doesNotMatch(html, /id="agent-proxy"/);
  assert.match(html, /id="gateway-client"/);
  const js = fs.readFileSync(path.join(__dirname, '../ui/client-connect.js'), 'utf8');
  assert.match(js, /'接入本地网关'/);
  assert.match(js, /class: 'panelhead'/, '标题行要挂 .panelhead，字体才跟原来的「模型反代」一致');
  assert.match(js, /class: 'sub'/);
  const css = fs.readFileSync(path.join(__dirname, '../ui/client-connect.css'), 'utf8');
  assert.doesNotMatch(css, /\.gateway-client h2\s*\{[^}]*font-size/, '不要再用 16px 覆盖 panelhead 的字体');
});

test('折叠记账：默认全展开，收起的块记进 localStorage', () => {
  const store = {};
  const c = context({
    localStorage: {
      getItem: (k) => (k in store ? store[k] : null),
      setItem: (k, v) => { store[k] = v; },
    },
  });
  for (const name of ['pxFoldMap', 'pxFolded', 'pxSetFold', 'pxUnfold']) vm.runInContext(extract(name), c);
  assert.equal(c.pxFolded('models'), false, '默认展开');
  c.pxSetFold('keys', true);
  assert.equal(c.pxFolded('keys'), true);
  assert.deepEqual(JSON.parse(store['cockpit-proxy-fold']), { keys: true });
  // 换一个"会话"重来，状态还在
  c.S.proxyPage.foldMap = null;
  assert.equal(c.pxFolded('keys'), true, '重开驾驶舱要记得你收起了哪一块');
  c.pxUnfold('keys');
  assert.equal(c.pxFolded('keys'), false);
  assert.deepEqual(JSON.parse(store['cockpit-proxy-fold']), {});
});
