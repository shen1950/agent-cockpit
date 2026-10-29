/* Real browser regression for account-detail chart scrolling. All data is synthetic. */
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const { chromium } = require('D:/npm-global/node_modules/@playwright/mcp/node_modules/playwright');

const root = path.resolve(__dirname, '../ui');
const mime = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.svg': 'image/svg+xml', '.png': 'image/png' };
const server = http.createServer((req, res) => {
  const requested = decodeURIComponent(new URL(req.url, 'http://localhost').pathname);
  const file = path.resolve(root, '.' + (requested === '/' ? '/index.html' : requested));
  if (file !== root && !file.startsWith(root + path.sep)) { res.writeHead(403).end(); return; }
  fs.readFile(file, (error, bytes) => {
    if (error) { res.writeHead(404).end(); return; }
    res.setHeader('Content-Type', mime[path.extname(file)] || 'application/octet-stream'); res.end(bytes);
  });
});

async function frames(page) {
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
}

async function seed(page, app) {
  await page.evaluate((app) => {
    clearInterval(pollTimer);
    const original = structuredClone(window.scrollFixtureSource ||= S.data.instances[0]);
    const date = new Date();
    const days = [];
    for (let offset = 60; offset >= 0; offset--) {
      const d = new Date(date.getFullYear(), date.getMonth(), date.getDate() - offset);
      const key = `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
      days.push({ date: key, credits: (offset + 1) * 100, sessions: 1 });
    }
    const trend = { unit: 'tokens', days, total: days.reduce((n, d) => n + d.credits, 0),
      peak: 6100, peak_date: days[0].date, source: '合成数据' };
    const make = name => ({ ...structuredClone(original), app, app_name: app, name,
      kind: app === 'codex-desktop' ? 'pwa' : 'electron', running: false, resident: false,
      logged_in: false, swapped: false, in_place: false,
      quota: { ...original.quota, groups: [], credits: [], left: null, total: null, trend },
      chatgpt_auth: { has_auth: true, mode: 'chatgpt', account: 'fixture@example.test' },
      chatgpt_quota: { state: 'live', mode: 'chatgpt', windows: [] },
      zcode_auth: { provider: 'zai', account: name, has_auth: true, current: false, auth_status: 'ready' },
    });
    S.data.apps = [{ id: app, name: app, kind: 'electron', installed: true,
      isolatable: true, swap_ok: false, local_quota: false, web_quota: false }];
    S.data.instances = [make('测试账号甲'), make('测试账号乙')];
    const fixture = S.data;
    invoke = async (command, args = {}) => command === 'get_state' ? fixture
      : command === 'get_meta' ? fixture.instances.find(i => i.name === args.name) || {} : { ok: true };
    S.view = 'agents'; S.cur = app; S.busy = false; render();
  }, app);
  // Keep a large scrollable section and a narrow heatmap so both axes can be tested.
  await page.addStyleTag({ content: '#im-body .trend { margin-bottom: 500px !important; } #im-body .heatwrap { max-width: 260px !important; }' });
  await page.evaluate(async (app) => openDetail(app, '测试账号甲', 'quota'), app);
  await frames(page);
}

async function position(page) {
  return page.evaluate(() => ({ top: document.getElementById('im-body').scrollTop,
    left: document.querySelector('#im-body .heatwrap')?.scrollLeft ?? -1 }));
}
async function assertStable(page, action, expected, label) {
  await action();
  await frames(page);
  const next = await position(page);
  assert.ok(Math.abs(next.top - expected.top) <= 2, `${label}: vertical ${expected.top} -> ${next.top}`);
  assert.ok(Math.abs(next.left - expected.left) <= 2, `${label}: horizontal ${expected.left} -> ${next.left}`);
  return next;
}

test('usage chart interaction and polling preserve the correct detail scroll', async () => {
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  let browser;
  try {
    browser = await chromium.launch({ channel: 'chrome', headless: true });
    const page = await browser.newPage({ viewport: { width: 1024, height: 640 } });
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.addInitScript(() => localStorage.setItem('cockpit-toured', '1'));
    await page.goto(`http://127.0.0.1:${server.address().port}/`, { waitUntil: 'networkidle' });
    await page.waitForFunction(() => typeof S !== 'undefined' && S.data && !S.busy);
    for (const app of ['codex-desktop', 'zcode', 'opencode', 'antigravity']) {
      await seed(page, app);
      assert.equal(await page.locator('#im-body .wave .whit').count() > 0, true, `${app} has a curve`);
      const expected = await page.evaluate(() => {
        const body = document.getElementById('im-body'), heat = body.querySelector('.heatwrap');
        body.scrollTop = Math.min(160, body.scrollHeight - body.clientHeight);
        heat.scrollLeft = Math.min(180, heat.scrollWidth - heat.clientWidth);
        return { top: body.scrollTop, left: heat.scrollLeft };
      });
      assert.ok(expected.top > 50, `${app} fixture must be vertically scrollable`);
      assert.ok(expected.left > 40, `${app} fixture must be horizontally scrollable`);
      // Invoke the native click handlers within the page without Playwright scrolling
      // the element into view, so the assertion measures application behaviour.
      const click = selector => page.evaluate(sel => {
        const el = document.querySelector(sel); if (!el) throw new Error(`missing ${sel}`);
        el.dispatchEvent(new MouseEvent('click', { bubbles: true }));
      }, selector);
      let current = await assertStable(page, () => click('#im-body .wave .whit'), expected, `${app} wave click`);
      assert.ok(await page.locator('#im-body .wguide').count(), `${app} selected wave has guide`);
      current = await assertStable(page, () => click('#im-body .heat .hc:not(.void)'), current, `${app} heat click`);
      current = await assertStable(page, () => click('#im-body .trpick .tchip'), current, `${app} clear selection`);
      current = await assertStable(page, () => click('#im-body .trrange .tchip'), current, `${app} range`);
      await assertStable(page, () => page.evaluate(() => renderDetail()), current, `${app} periodic redraw`);
      await page.evaluate(() => document.querySelector('#im-nav button').click());
      await frames(page);
      assert.equal(await page.locator('#im-body').evaluate(e => e.scrollTop), 0, `${app} switch section starts at top`);
      await page.evaluate(async app => openDetail(app, '测试账号甲', 'quota'), app);
      await frames(page);
      await page.locator('#im-body').evaluate(e => { e.scrollTop = 150; });
      await page.evaluate(async app => openDetail(app, '测试账号乙', 'quota'), app);
      await frames(page);
      assert.equal(await page.locator('#im-body').evaluate(e => e.scrollTop), 0, `${app} switching account starts at top`);
      await page.evaluate(() => closeDetailNow());
    }
    assert.deepEqual(errors, [], 'no frontend runtime errors');
  } finally {
    if (browser) await browser.close();
    await new Promise(resolve => server.close(resolve));
  }
});
