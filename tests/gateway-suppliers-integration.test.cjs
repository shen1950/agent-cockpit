/* Browser smoke test for the Magpie-style supplier page and key admission modal. */
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const { chromium } = require('playwright');

const root = path.resolve(__dirname, '../ui');
const evidence = path.resolve(__dirname, '../.agent-sync/gateway-supplier-evidence');
const mime = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.svg': 'image/svg+xml', '.png': 'image/png', '.jpg': 'image/jpeg' };
const server = http.createServer((req, res) => {
  const pathname = decodeURIComponent(new URL(req.url, 'http://localhost').pathname);
  const file = path.resolve(root, '.' + (pathname === '/' ? '/index.html' : pathname));
  if (!file.startsWith(root + path.sep)) { res.writeHead(403).end(); return; }
  fs.readFile(file, (error, bytes) => {
    if (error) { res.writeHead(404).end(); return; }
    res.setHeader('Content-Type', mime[path.extname(file)] || 'application/octet-stream');
    res.end(bytes);
  });
});

(async () => {
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  fs.mkdirSync(evidence, { recursive: true });
  let browser;
  try {
    browser = await chromium.launch({ channel: 'chrome', headless: true });
    const page = await browser.newPage({ viewport: { width: 900, height: 1000 } });
    const errors = [];
    page.on('pageerror', (error) => errors.push(error.message));
    await page.addInitScript(() => localStorage.setItem('cockpit-toured', '1'));
    await page.goto(`http://127.0.0.1:${server.address().port}/`, { waitUntil: 'domcontentloaded' });
    await page.waitForFunction(() => typeof S !== 'undefined' && S.data);
    await page.evaluate(() => { S.view = 'proxy'; render(); });
    await page.waitForSelector('#proxy-suppliers .gateway-supplier-item');

    const modal = page.locator('#gdmodal');
    await page.evaluate(() => openDistModal(null));
    const providerNames = await modal.locator('.gpmem .gpmem-name').allInnerTexts();
    assert.ok(providerNames.length > 0, 'key modal must list providers');
    assert.equal(await modal.locator('.gpmem').count(), providerNames.length, 'disabled providers must be hidden from admission');
    assert.ok(providerNames.every((name) => !name.includes('已停用')), providerNames.join(', '));
    await page.screenshot({ path: path.join(evidence, 'key-provider-admission.png') });
    await page.keyboard.press('Escape');
    await page.waitForFunction(() => document.getElementById('gdmodal').hidden);

    await page.click('#proxy-suppliers .gateway-supplier-row');
    await page.waitForSelector('.gateway-supplier-detail');
    await page.selectOption('.gateway-supplier-detail select[aria-label="接入密钥"]', { index: 1 });
    await page.waitForSelector('.gateway-supplier-detail select[aria-label="设置模型"]');
    assert.equal(await page.locator('.gateway-supplier-detail select[aria-label="设置模型"] option').count(), 3);
    assert.ok(await page.getByLabel('图片输入', { exact: true }).isVisible());
    assert.ok(await page.getByLabel('上下文窗口', { exact: true }).isVisible());
    assert.ok(await page.getByLabel('最大输出', { exact: true }).isVisible());
    assert.ok(await page.getByLabel('压缩阈值', { exact: true }).isVisible());
    assert.ok(await page.getByLabel('推理思考', { exact: true }).isVisible());
    assert.ok(await page.getByLabel('每个账号的并发上限', { exact: true }).isVisible());
    assert.ok(await page.getByLabel('价格倍率', { exact: true }).isVisible());
    await page.getByLabel('上下文窗口', { exact: true }).fill('1M');
    await page.getByRole('button', { name: '保存模型设置', exact: true }).click();
    const savedState = await page.evaluate(() => S?.data?.proxy?.mockPolicies);
    await page.waitForFunction(() => S?.data?.proxy?.mockPolicies?.['mock/vision']?.context_window === 1000000);
    await page.screenshot({ path: path.join(evidence, 'supplier-detail-settings.png'), fullPage: true });
    assert.deepEqual(errors, [], `page must not throw: ${errors.join(' | ')}`);
    console.log(JSON.stringify({ result: 'PASS', evidence }));
  } finally {
    await browser?.close();
    await new Promise((resolve) => server.close(resolve));
  }
})().catch((error) => { console.error(error); process.exitCode = 1; });
