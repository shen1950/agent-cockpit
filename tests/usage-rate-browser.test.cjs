/* Browser regression for historical rate controls. All usage is synthetic. */
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

test('long-range chart uses recorded buckets, filtered scope and updates focused controls', async () => {
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  let browser;
  try {
    browser = await chromium.launch({ channel: 'chrome', headless: true });
    const page = await browser.newPage({ viewport: { width: 1320, height: 850 } });
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.addInitScript(() => {
      localStorage.setItem('cockpit-toured', '1');
      localStorage.setItem('cockpit-usage-preferences', JSON.stringify({ interval: 0 }));
    });
    await page.goto(`http://127.0.0.1:${server.address().port}/`, { waitUntil: 'networkidle' });
    await page.waitForFunction(() => typeof S !== 'undefined' && S.data && !S.busy);
    await page.evaluate(() => {
      clearInterval(pollTimer);
      // Two separated 5-minute buckets within the previous complete hour.
      const now = Date.now() / 1000, first = Math.floor(now / 3600) * 3600 - 3600 + 600;
      const b = (timestamp, exact_tokens, estimated_tokens = 0) => ({ timestamp, exact_tokens, estimated_tokens, events: 1 });
      const common = [b(first, 1500, 300), b(first + 600, 4500)];
      const own = [b(first, 2500), b(first + 600, 6000)];
      const day = new Date();
      const date = `${day.getFullYear()}-${String(day.getMonth() + 1).padStart(2, '0')}-${String(day.getDate()).padStart(2, '0')}`;
      const rows = [{ date, tokens: 6300, exact_tokens: 6000, estimated_tokens: 300, credits: 6300, models: [] }];
      const sources = [
        { id: 'codex', app_id: 'codex', app_name: 'Codex', unit: 'tokens', days: rows, status: 'ready', count_in_total: true, historical_5m: common },
        { id: 'desktop', app_id: 'codex-desktop', app_name: 'ChatGPT', instance_name: '本机共用记录',
          scope: 'device', unit: 'tokens', days: [], status: 'duplicate', count_in_total: false,
          app_days: rows, app_status: 'ready', source_days: rows, source_status: 'ready',
          historical_5m: [], app_historical_5m: common, source_historical_5m: own },
      ];
      const sample = (rate) => ({ tokens_per_minute: rate, exact_tokens_per_minute: rate, estimated_tokens_per_minute: 0,
        history: [{ timestamp: now - 20, tokens_per_minute: rate, exact_tokens_per_minute: rate, estimated_tokens_per_minute: 0 }] });
      const payload = { generated_at: now, sources, monitor: { samples: 2, interval_seconds: 30, ...sample(9),
        by_source: { codex: sample(9) }, by_app_source: { desktop: sample(35) }, by_selected_source: { desktop: sample(60) } } };
      invoke = async command => command === 'usage_dashboard' ? payload : { ok: true };
      S.view = 'usage'; render();
    });
    await page.waitForSelector('#ud-rate-range');
    await page.waitForFunction(() => document.querySelectorAll('.ud-rate-point').length === 2);
    let points = page.locator('.ud-rate-chart .ud-rate-point');
    assert.match(await points.nth(0).getAttribute('aria-label'), /平均 360 Token\/分钟/);
    assert.match(await points.nth(1).getAttribute('aria-label'), /平均 900 Token\/分钟/);
    assert.equal(await page.locator('.ud-rate-chart line[stroke]').count(), 0, 'missing 5-minute interval does not create a fake connecting line');
    await points.nth(0).click();
    assert.match(await page.locator('.ud-rate-picked').textContent(), /平均 360 Token\/分钟/);

    // A range selection originates inside the chart; keep focus and update immediately.
    const changeRange = async (value) => page.$eval('#ud-rate-range', (control, v) => {
      control.focus(); control.value = v; control.dispatchEvent(new Event('change', { bubbles: true }));
      return document.activeElement?.id;
    }, value);
    const scroller = page.locator('#view-usage .pagebody');
    await scroller.evaluate(el => { el.scrollTop = el.scrollHeight; });
    const before = await scroller.evaluate(el => el.scrollTop);
    assert.ok(before > 0, 'fixture must scroll');
    assert.equal(await changeRange('7d'), 'ud-rate-range', 'range retains focus after changing');
    points = page.locator('.ud-rate-chart .ud-rate-point');
    assert.equal(await points.count(), 1, '7-day view aggregates both buckets into their hour immediately');
    assert.match(await points.nth(0).getAttribute('aria-label'), /平均 105 Token\/分钟/);
    assert.ok(Math.abs((await scroller.evaluate(el => el.scrollTop)) - before) <= 2, 'range change keeps scroll');
    assert.equal(await changeRange('30d'), 'ud-rate-range');
    assert.match(await page.locator('.ud-rate-chart .ud-rate-point').first().getAttribute('aria-label'), /平均 4\.38 Token\/分钟/);
    assert.equal(await changeRange('live'), 'ud-rate-range');
    assert.match(await page.locator('.ud-rate-chart .ud-rate-point').first().getAttribute('aria-label'), /9 Token\/分钟/);

    await page.selectOption('#ud-agent', 'codex-desktop');
    assert.match(await page.locator('.ud-rate-chart .ud-rate-point').first().getAttribute('aria-label'), /35 Token\/分钟/);
    await changeRange('24h');
    assert.match(await page.locator('.ud-rate-chart .ud-rate-point').first().getAttribute('aria-label'), /平均 360 Token\/分钟/);
    await page.selectOption('#ud-source', 'desktop');
    assert.match(await page.locator('.ud-rate-chart .ud-rate-point').first().getAttribute('aria-label'), /平均 500 Token\/分钟/);
    await changeRange('live');
    assert.match(await page.locator('.ud-rate-chart .ud-rate-point').first().getAttribute('aria-label'), /60 Token\/分钟/);
    assert.deepEqual(errors, [], 'no frontend runtime errors');
  } finally {
    if (browser) await browser.close();
    await new Promise(resolve => server.close(resolve));
  }
});
