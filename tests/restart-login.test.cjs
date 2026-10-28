/* Exercise the shipped confirmation dialog in Chromium. A mocked IPC session
   deliberately remains at waiting/closing, so no desktop login or account is touched. */
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const { chromium } = require('D:/npm-global/node_modules/@playwright/mcp/node_modules/playwright');
const root = path.resolve(__dirname, '../ui');
const evidence = path.resolve(__dirname, '../.agent-sync/layout-evidence');
const server = http.createServer((req, res) => {
  const pathname = decodeURIComponent(new URL(req.url, 'http://localhost').pathname);
  const file = path.resolve(root, '.' + (pathname === '/' ? '/index.html' : pathname));
  if (!file.startsWith(root + path.sep)) return res.writeHead(403).end();
  fs.readFile(file, (err, bytes) => {
    if (err) return res.writeHead(404).end();
    const type = { '.html': 'text/html', '.css': 'text/css', '.js': 'text/javascript' }[path.extname(file)];
    if (type) res.setHeader('Content-Type', type);
    res.end(bytes);
  });
});
async function seed(page, scale) {
  await page.evaluate(scale => {
    clearInterval(pollTimer);
    S.data.settings.appearance = { ...S.data.settings.appearance, font_scale: scale };
    render();
    window.loginRequests = [];
    window.loginSession = { id: 'abcd-1234', phase: 'waiting', running: true,
      imported: false, message: '请在临时官方客户端中完成登录' };
    invoke = async (command, args = {}) => {
      if (command === 'get_state') return S.data;
      const action = args.payload?.action;
      window.loginRequests.push(action);
      if (action === 'chatgpt_official_cancel') {
        window.loginSession.phase = 'closing';
        window.loginSession.message = '正在关闭本次登录的临时客户端';
      }
      return { ok: true, session: { ...window.loginSession } };
    };
    openChatgptAdd({ id: 'codex-desktop' });
  }, scale);
  await page.getByRole('button', { name: '开始官方登录', exact: true }).click();
}
async function checkConfirmation(page, label) {
  await page.waitForFunction(() => !document.getElementById('ask').hidden);
  const hit = await page.locator('#ask-ok').evaluate(button => {
    const r = button.getBoundingClientRect();
    const target = document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2);
    return { reachable: button === target || button.contains(target), interceptedBy: target?.className };
  });
  await page.screenshot({ path: path.join(evidence, `login-confirm-${label}.png`) });
  assert.ok(hit.reachable, `confirmation must receive pointer input: ${JSON.stringify(hit)}`);
}
(async () => {
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  fs.mkdirSync(evidence, { recursive: true });
  let browser;
  try {
    browser = await chromium.launch({ channel: 'chrome', headless: true });
    for (const [width, height, scale] of [[1320, 920, 100], [960, 850, 125]]) {
      for (const trigger of ['取消本次登录', '关闭']) {
        const page = await browser.newPage({ viewport: { width, height } });
        await page.addInitScript(() => localStorage.setItem('cockpit-toured', '1'));
        await page.goto(`http://127.0.0.1:${server.address().port}/`, { waitUntil: 'networkidle' });
        await page.waitForFunction(() => typeof S !== 'undefined' && S.data && !S.busy);
        await seed(page, scale);
        const button = page.locator('.cpt-overlay').getByRole('button', { name: trigger, exact: true });
        await button.click();
        await checkConfirmation(page, `${scale}-${trigger === '关闭' ? 'close' : 'cancel'}`);
        // Rejecting cancellation must leave the session alone.
        await page.locator('#ask-cancel').click();
        assert.equal(await page.evaluate(() => loginRequests.includes('chatgpt_official_cancel')), false);
        assert.equal(await page.locator('.cpt-overlay').count(), 1);
        // Confirm by a real pointer click, then allow asynchronous cleanup to finish.
        await button.click();
        await page.locator('#ask-ok').click();
        await page.waitForFunction(() => loginRequests.includes('chatgpt_official_cancel'));
        await page.evaluate(() => Object.assign(loginSession, {
          phase: 'cancelled', running: false, message: '本次登录已取消'
        }));
        if (trigger === '关闭') await page.locator('.cpt-overlay').waitFor({ state: 'detached' });
        else {
          await page.getByRole('button', { name: '开始官方登录', exact: true }).waitFor({ state: 'visible' });
          await page.locator('.cpt-overlay').getByRole('button', { name: '关闭', exact: true }).click();
          await page.locator('.cpt-overlay').waitFor({ state: 'detached' });
        }
        console.log(`PASS: ${trigger}, ${width}x${height}, font ${scale}%: confirmation clickable, reject safe, cancel/cleanup completes`);
        await page.close();
      }
    }
  } finally { await browser?.close(); await new Promise(resolve => server.close(resolve)); }
})().catch(error => { console.error(error); process.exitCode = 1; });
