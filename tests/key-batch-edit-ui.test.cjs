/* 批量编辑弹窗：用本机预览桥（mock.js）真渲染一遍，确认
   ① 工具栏里「批量编辑」点了能开、弹窗里按站点列出全部密钥；
   ② 勾一条密钥 + 勾「名称」并填值，点应用只发一次 key_batch_update，
      且 patch 里只有 label（没勾的项一个都不带）；
   ③ 什么都没勾时「应用」是灰的。
   截图落在 .agent-sync/key-batch-edit-evidence/。 */
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const { chromium } = require('playwright');

const root = path.resolve(__dirname, '../ui');
const evidence = path.resolve(__dirname, '../.agent-sync/key-batch-edit-evidence');
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
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  fs.mkdirSync(evidence, { recursive: true });
  let browser;
  try {
    browser = await chromium.launch({ channel: 'chrome', headless: true });
    const page = await browser.newPage({ viewport: { width: 1320, height: 900 } });
    const errors = [];
    page.on('pageerror', (e) => errors.push(e.message));
    await page.addInitScript(() => localStorage.setItem('cockpit-toured', '1'));
    await page.goto(`http://127.0.0.1:${server.address().port}/`, { waitUntil: 'networkidle' });
    await page.waitForFunction(() => typeof S !== 'undefined' && S.data && !S.busy);
    await page.evaluate(() => { S.view = 'keys'; render(); });

    // ① 入口在工具栏里，不用先勾选
    const entry = page.locator('#btn-keys-edit');
    assert.equal(await entry.isVisible(), true, '工具栏里应有「批量编辑」');
    assert.equal((await entry.textContent()).trim(), '批量编辑');
    await page.screenshot({ path: path.join(evidence, 'toolbar.png'), clip: { x: 0, y: 0, width: 1320, height: 160 } });
    await entry.click();
    await page.waitForSelector('#bemodal:not([hidden])');
    const rows = page.locator('#be-list .bdrow');
    assert.ok(await rows.count() > 0, '弹窗里应列出密钥');

    // ③ 还没勾：应用按钮灰着
    assert.equal(await page.locator('#be-go').isDisabled(), true);
    assert.match(await page.locator('#be-count').textContent(), /先在上面勾/);

    // ② 勾一条 + 勾「名称」填值 → 应用只发一条 key_batch_update，patch 只有 label
    const sent = [];
    await page.exposeFunction('__record', (p) => sent.push(p));
    // invoke 是 app.js 里的顶层 let（全局词法绑定，不挂 window），只能直接赋值替换
    await page.evaluate(() => {
      const orig = invoke;
      invoke = async (cmd, arg) => {
        if (cmd === 'action' && arg && arg.payload && arg.payload.action === 'key_batch_update') {
          window.__record(arg.payload);
          return { ok: true, msg: '已改 1 条' };
        }
        return orig(cmd, arg);
      };
    });
    await rows.first().locator('input[type="checkbox"]').check();
    const nameRow = page.locator('#be-form .berow').first();
    await nameRow.locator('.be-on').check();
    await nameRow.locator('.be-val').fill('批量改的名字');
    assert.equal(await page.locator('#be-go').isDisabled(), false);
    assert.match(await page.locator('#be-count').textContent(), /将改 1 条密钥的 1 项/);
    await page.screenshot({ path: path.join(evidence, 'modal.png') });
    await page.locator('#be-go').click();
    await page.waitForSelector('#bemodal', { state: 'hidden' });
    assert.equal(sent.length, 1, '应只发一条 key_batch_update');
    assert.equal(sent[0].ids.length, 1);
    assert.equal(JSON.stringify(sent[0].patch), JSON.stringify({ label: '批量改的名字' }));
    assert.equal(errors.length, 0, `页面报错：${errors.join(' / ')}`);
    console.log('ok 批量编辑弹窗：入口 / 勾选 / 只改勾上的项');
  } finally {
    if (browser) await browser.close();
    server.close();
  }
})().catch((e) => { console.error(e); process.exit(1); });
