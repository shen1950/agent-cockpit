// Node VM 中运行真实 load、侧栏排序和首页拖拽模块，手动控制响应顺序。
// 不启动 Chrome，不读取本机账号或配置。
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const app = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');
const sort = fs.readFileSync(path.join(__dirname, '../ui/home-agent-sort.js'), 'utf8');
const loadSource = app.slice(app.indexOf('async function load() {'), app.indexOf('/* ---------------- 渲染总入口'));
const navSource = app.slice(app.indexOf('function applyNavSort('), app.indexOf('/* 视图切换只换右栏内容'));
const plain = value => JSON.parse(JSON.stringify(value));
const flush = () => new Promise(resolve => setImmediate(resolve));
const initial = ['qoder-cn', 'miaoshou', 'workbuddy'];
const moved = ['miaoshou', 'qoder-cn', 'workbuddy'];
function snapshot(ids = initial, count = 1) {
  return { apps: ids.map(id => ({ id, name: id })), stats: { count },
    settings: { app_order: [...ids], appearance: { theme: 'light' } } };
}
function harness() {
  const reads = [], writes = [], frames = [], notices = [];
  let nodes = [], hit = null;
  const events = () => {
    const map = new Map();
    return {
      addEventListener(type, fn) { if (!map.has(type)) map.set(type, new Set()); map.get(type).add(fn); },
      removeEventListener(type, fn) { map.get(type)?.delete(fn); },
      emit(type, e) { for (const fn of [...(map.get(type) || [])]) fn(e); },
    };
  };
  const classes = () => {
    const values = new Set();
    return { add: (...names) => names.forEach(n => values.add(n)),
      remove: (...names) => names.forEach(n => values.delete(n)) };
  };
  const host = { ...events(), closest: () => null,
    querySelectorAll: () => nodes };
  const document = { ...events(), getElementById: () => host,
    elementFromPoint: () => hit,
    body: { classList: classes(), appendChild() {} },
    createElement: () => ({ style: {}, offsetWidth: 80, setAttribute() {}, remove() {} }),
  };
  const ctx = vm.createContext({
    S: { data: snapshot(), busy: false, view: 'home', cur: initial[0] },
    lastSettingsWrite: 0, document, window: events(), innerWidth: 1320,
    requestAnimationFrame: () => 1, cancelAnimationFrame() {},
    MutationObserver: class { observe() {} },
    toast: (...args) => notices.push(args),
    invoke(cmd, args) {
      if (cmd === 'get_state') {
        return new Promise((resolve, reject) => reads.push({ resolve, reject }));
      }
      assert.equal(cmd, 'action');
      assert.equal(args.payload.action, 'app_reorder');
      return new Promise((resolve, reject) => writes.push({ payload: plain(args.payload), resolve, reject }));
    },
    render() {
      frames.push(plain(ctx.S.data));
      nodes = ctx.S.data.apps.filter(a => !a.hidden).map((a, i) => {
        const node = { title: a.name, classList: classes(),
          getBoundingClientRect: () => ({ left: i * 100, top: 0, width: 100, height: 50 }) };
        node.closest = () => node;
        return node;
      });
    },
  });
  vm.runInContext(loadSource + '\n' + navSource, ctx);
  ctx.render();
  vm.runInContext(fs.readFileSync(path.join(__dirname, '../ui/instance-presentation.js'), 'utf8'), ctx);
  vm.runInContext(sort, ctx);
  const pointer = (node, x) => ({ target: node, clientX: x, clientY: 25,
    pointerId: 1, pointerType: 'mouse', button: 0, isPrimary: true, preventDefault() {} });
  return { ctx, reads, writes, frames, notices,
    order: () => plain(ctx.S.data.apps.map(a => a.id)),
    drag(from = 0, to = 1) {
      const source = nodes[from]; hit = nodes[to];
      host.emit('pointerdown', pointer(source, from * 100 + 50));
      document.emit('pointermove', pointer(hit, to * 100 + 75));
      document.emit('pointerup', pointer(hit, to * 100 + 75));
    },
  };
}

test('刷新先读取旧顺序，拖拽保存后才返回：所有渲染保留新顺序，同时更新其他状态', async () => {
  const h = harness();
  const reading = h.ctx.load();
  h.drag();
  assert.deepEqual(h.writes[0].payload.ids, moved);
  h.writes[0].resolve({ ok: true }); await flush();
  const start = h.frames.length;
  assert.deepEqual(h.order(), moved);
  h.reads[0].resolve(snapshot(initial, 99)); await reading;
  assert.deepEqual(h.order(), moved, '晚返回的旧快照不能让卡片回位');
  assert.equal(h.ctx.S.data.stats.count, 99, '运行状态等其他新数据仍须刷新');
  assert.deepEqual(plain(h.ctx.S.data.settings.app_order), moved);
  for (const frame of h.frames.slice(start)) assert.deepEqual(frame.apps.map(a => a.id), moved);
});

test('保存尚未完成时发出的刷新同样不能在保存成功后覆盖顺序', async () => {
  const h = harness(); h.drag();
  const reading = h.ctx.load();
  h.writes[0].resolve({ ok: true }); await flush();
  h.reads[0].resolve(snapshot()); await reading;
  assert.deepEqual(h.order(), moved);
});

test('刷新在保存之前返回，随后成功保存仍正确，后续正常刷新继续生效', async () => {
  const h = harness(); const reading = h.ctx.load(); h.drag();
  h.reads[0].resolve(snapshot(initial, 2)); await reading;
  h.writes[0].resolve({ ok: true }); await flush();
  assert.deepEqual(h.order(), moved);
  const fresh = h.ctx.load();
  const external = ['workbuddy', 'miaoshou', 'qoder-cn'];
  h.reads[1].resolve(snapshot(external, 3)); await fresh;
  assert.deepEqual(h.order(), external, '未跨越本机排序保存的新请求应采用后端真值');
});

test('忙碌时重复调用 load 不得清除原请求的排序保护', async () => {
  const h = harness(); const reading = h.ctx.load(); h.drag();
  h.writes[0].resolve({ ok: true }); await flush();
  await h.ctx.load(); assert.equal(h.reads.length, 1);
  h.reads[0].resolve(snapshot()); await reading;
  assert.deepEqual(h.order(), moved);
});

test('同一旧刷新跨越两次成功排序，以最后一次保存的顺序为准', async () => {
  const h = harness(); const reading = h.ctx.load();
  h.drag(); h.writes[0].resolve({ ok: true }); await flush();
  h.drag(0, 2); h.writes[1].resolve({ ok: true }); await flush();
  const latest = h.writes[1].payload.ids;
  h.reads[0].resolve(snapshot()); await reading;
  assert.deepEqual(h.order(), latest);
});

test('旧快照带来新增或移除程序时只调整相对顺序，不复活旧卡片', async () => {
  const h = harness(); const reading = h.ctx.load(); h.drag();
  h.writes[0].resolve({ ok: true }); await flush();
  h.reads[0].resolve(snapshot(['qoder-cn', 'new-app', 'miaoshou'])); await reading;
  assert.deepEqual(h.order(), ['miaoshou', 'qoder-cn', 'new-app']);
});

for (const failure of ['rejected', 'throw']) test(`保存失败不留下虚假的新顺序：${failure}`, async () => {
  const h = harness(); const reading = h.ctx.load(); h.drag();
  if (failure === 'throw') h.writes[0].reject(new Error('mock unavailable'));
  else h.writes[0].resolve({ ok: false, msg: 'mock refused' });
  await flush();
  h.reads[0].resolve(snapshot()); await reading;
  assert.deepEqual(h.order(), initial);
  assert.ok(h.notices.some(([text]) => text.includes('排序未保存')));
});

test('侧栏与首页共用保存保护，交错排序不会被旧刷新覆盖', async () => {
  const h = harness(); const reading = h.ctx.load();
  h.ctx.applyNavSort('qoder-cn', 'miaoshou', true);
  h.writes[0].resolve({ ok: true }); await flush();
  h.reads[0].resolve(snapshot()); await reading;
  assert.deepEqual(h.order(), moved);
});

test('刷新失败后释放保护，后续读取仍接受后端最新顺序', async () => {
  const h = harness(); const reading = h.ctx.load(); h.drag();
  h.writes[0].resolve({ ok: true }); await flush();
  h.reads[0].reject(new Error('mock read failed')); await reading;
  assert.deepEqual(h.order(), moved);
  const fresh = h.ctx.load();
  h.reads[1].resolve(snapshot(initial)); await fresh;
  assert.deepEqual(h.order(), initial);
});
