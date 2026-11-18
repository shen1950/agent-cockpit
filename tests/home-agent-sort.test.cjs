// 离线 L2：真实鼠标拖拽，后端替身只实现已有 app_reorder/get_state 契约。
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const { chromium } = require('playwright');
const root = path.resolve(__dirname, '../ui');
const evidence = path.resolve(__dirname, '../.agent-sync/codex-home-card-sort-20261117-231152-de95-evidence');
const mime = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.svg': 'image/svg+xml', '.png': 'image/png' };
const bridge = `
(() => {
  const base = window.__TAURI__.core.invoke;
  window.sortWrites = [];
  window.hideTestApp = () => { DATA.apps[1].hidden = true; };
  const restore = () => {
    const ids = JSON.parse(localStorage.getItem('test.app-order') || '[]');
    const pos = new Map(ids.map((id, i) => [id, i]));
    DATA.apps.sort((a, b) => (pos.get(a.id) ?? ids.length) - (pos.get(b.id) ?? ids.length));
  };
  restore();
  window.__TAURI__.core.invoke = async (cmd, args) => {
    if (cmd === 'action' && args?.payload?.action === 'app_reorder') {
      window.sortWrites.push(args.payload);
      if (window.rejectSort === 'throw') throw new Error('模拟连接中断');
      if (window.rejectSort) return { ok: false, msg: '模拟保存失败' };
      localStorage.setItem('test.app-order', JSON.stringify(args.payload.ids));
      restore();
      return { ok: true };
    }
    return base(cmd, args);
  };
})();`;
const server = http.createServer((req, res) => {
  const url = decodeURIComponent(new URL(req.url, 'http://localhost').pathname);
  if (url === '/favicon.ico') { res.writeHead(204).end(); return; }
  const file = path.resolve(root, '.' + (url === '/' ? '/index.html' : url));
  if (!file.startsWith(root + path.sep)) { res.writeHead(403).end(); return; }
  fs.readFile(file, (err, bytes) => {
    if (err) { res.writeHead(404).end(); return; }
    res.setHeader('Content-Type', mime[path.extname(file)] || 'application/octet-stream');
    res.end(path.basename(file) === 'mock.js' ? bytes.toString() + bridge : bytes);
  });
});

(async () => {
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  fs.mkdirSync(evidence, { recursive: true });
  let browser;
  try {
    browser = await chromium.launch({ channel: 'chrome', headless: true });
    for (const viewport of [{ width: 1320, height: 860 }, { width: 940, height: 620 }]) {
      const page = await browser.newPage({ viewport });
      const errors = [];
      page.on('pageerror', error => errors.push(error.message));
      page.on('console', message => { if (message.type() === 'error') errors.push(`${message.text()} ${message.location().url}`); });
      await page.addInitScript(() => {
        localStorage.setItem('cockpit-toured', '1');
        localStorage.setItem('ac.expiringWarnedOn', new Date().toISOString().slice(0, 10));
      });
      await page.goto(`http://127.0.0.1:${server.address().port}/`, { waitUntil: 'networkidle' });
      await page.waitForFunction(() => typeof S !== 'undefined' && S.data && !S.busy);
      await page.evaluate(() => { S.view = 'home'; render(); });
      const grid = page.locator('.hagents');
      const cards = page.locator('.hagents .hacard');
      const order = () => page.evaluate(() => S.data.apps.map(a => a.id));
      const writes = () => page.evaluate(() => window.sortWrites.length);
      const point = async (i, fraction = .5) => {
        await cards.nth(i).scrollIntoViewIfNeeded();
        const b = await cards.nth(i).boundingBox();
        return { x: b.x + b.width * fraction, y: b.y + b.height / 2 };
      };
      const start = async i => {
        const p = await point(i);
        await page.mouse.move(p.x, p.y); await page.mouse.down();
      };
      const moveTo = async (i, fraction = .75) => {
        const p = await point(i, fraction);
        await page.mouse.move(p.x, p.y, { steps: 8 });
      };
      const synced = async () => {
        assert.deepEqual(await page.locator('#applist .nav-btn').evaluateAll(nodes => nodes.map(n => n.dataset.app)), await order());
        assert.equal(await page.evaluate(() => S.view), 'home', '拖完不能误打开 Agent');
        assert.equal(await page.locator('.ha-sort-ghost, .ha-drag-source, .ha-drop-before, .ha-drop-after').count(), 0);
      };
      const before = await order();
      assert.ok(before.length >= 5);

      // 同行从左到右；图标本身也能作为拖动起点。
      await cards.first().scrollIntoViewIfNeeded();
      const icon = await cards.first().locator('.fav').boundingBox();
      await page.mouse.move(icon.x + icon.width / 2, icon.y + icon.height / 2);
      await page.mouse.down(); await moveTo(1);
      await page.mouse.up();
      await page.waitForFunction(() => window.sortWrites.length === 1);
      assert.deepEqual((await order()).slice(0, 2), [before[1], before[0]]);
      await synced();

      // 跨行拖到末尾，途中让轮询真的重建一次 DOM。
      const prev = await order();
      await start(0); await moveTo(prev.length - 1);
      await page.evaluate(() => load());
      await moveTo(prev.length - 1);
      assert.equal(await page.locator('.ha-drop-after').count(), 1);
      await page.screenshot({ path: path.join(evidence, `drag-${viewport.width}.png`) });
      await page.mouse.up();
      await page.waitForFunction(() => window.sortWrites.length === 2);
      assert.deepEqual(await order(), [...prev.slice(1), prev[0]]);
      await synced();

      // 从末尾往前插入，再刷新页面验证替身持久化契约。
      await start(prev.length - 1); await moveTo(0, .25); await page.mouse.up();
      await page.waitForFunction(() => window.sortWrites.length === 3);
      assert.deepEqual(await order(), prev);
      await synced();
      await page.reload({ waitUntil: 'networkidle' });
      await page.waitForFunction(() => S.data && !S.busy);
      await page.evaluate(() => { S.view = 'home'; render(); });
      assert.deepEqual(await order(), prev);
      await synced();

      // Escape / pointercancel / 窗口失焦 / 卡片外松手 / 原位松手均不提交。
      for (const mode of ['escape', 'cancel', 'blur', 'outside', 'self']) {
        const n = await writes();
        await start(0); await moveTo(2);
        if (mode === 'escape') await page.keyboard.press('Escape');
        if (mode === 'cancel') await page.evaluate(() => document.dispatchEvent(new PointerEvent('pointercancel', { pointerId: 1 })));
        if (mode === 'blur') await page.evaluate(() => window.dispatchEvent(new Event('blur')));
        if (mode === 'outside') await page.mouse.move(4, 4);
        if (mode === 'self') await moveTo(0);
        await page.mouse.up();
        assert.equal(await writes(), n, mode);
        assert.deepEqual(await order(), prev, mode);
        assert.equal(await page.evaluate(() => S.view), 'home', `取消后仍在首页：${mode}`);
        await synced();
      }

      // 后端明确拒绝及 Promise 拒绝都要显示错误且保留顺序。
      for (const failure of [true, 'throw']) {
        await page.evaluate(value => { window.rejectSort = value; }, failure);
        await start(0); await moveTo(2); await page.mouse.up();
        await page.waitForFunction(() => document.getElementById('toast').textContent.includes('排序未保存'));
        assert.deepEqual(await order(), prev);
        await synced();
      }
      await page.evaluate(() => { window.rejectSort = false; });

      // 隐藏项不显示，但保存完整顺序时不能丢失。
      await page.evaluate(() => window.hideTestApp());
      await page.evaluate(() => load());
      const hiddenId = (await order())[1];
      const writesBeforeHidden = await writes();
      await start(0); await moveTo(2); await page.mouse.up();
      await page.waitForFunction(n => window.sortWrites.length > n, writesBeforeHidden);
      const last = await page.evaluate(() => window.sortWrites.at(-1));
      assert.equal(last.ids.length, before.length);
      assert.ok(last.ids.includes(hiddenId));
      await synced();

      // 小于阈值的抖动依然点击打开；下一次正常点击不应被拖拽遗留状态吞掉。
      await start(0);
      const p = await point(0);
      await page.mouse.move(p.x + 2, p.y + 1); await page.mouse.up();
      assert.equal(await page.evaluate(() => S.view), 'agents');
      await page.evaluate(() => { S.view = 'home'; render(); });
      await grid.scrollIntoViewIfNeeded();
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false, '窗口不应横向溢出');
      await page.screenshot({ path: path.join(evidence, `home-${viewport.width}.png`) });
      assert.deepEqual(errors, []);
      console.log(`PASS ${viewport.width}x${viewport.height}: 横向/跨行/向前排序、图标拖动、轮询、同步、持久化契约、取消、失败、隐藏项、点击`);
      await page.close();
    }
  } finally {
    await browser?.close();
    server.close();
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
