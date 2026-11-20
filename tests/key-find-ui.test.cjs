/* 找密钥面板：拿一段 key 反查它在哪条台账里。
   用本机预览桥（mock.js）真渲染一遍，确认：
   ① 面板在密钥管理页里，输入框与按钮都在；
   ② 命中后按三档分开展示（完全一致 / 打码一致 / 片段匹配），
      且每条都说清「在哪个站点、哪一条密钥」；
   ③ 查不到时如实说没找到，而不是空白；
   ④ 「在台账里定位」会展开那个站点并把目标行点亮。
   截图落在 .agent-sync/key-find-evidence/。 */
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const { chromium } = require('playwright');

const root = path.resolve(__dirname, '../ui');
const evidence = path.resolve(__dirname, '../.agent-sync/key-find-evidence');
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

// 假台账：一把完整原文、一把只有打码、一把长 key。
// 站点 id / key id 必须和 ui/mock.js 里的台账对得上，
// 否则「在台账里定位」找不到那一行——那正是这个测试要验的。
const FAKE = {
  ok: true,
  locked: false,
  query: 'sk-testfull123456',
  exact: [{
    site: 'p-relay-a', site_name: '某中转站 A', site_kind: 'relay',
    homepage: 'https://a.example.com', base_url: 'https://api.relay-a.example/v1',
    id: 'k2', label: '备用', masked: 'sk-rel••••9c1d',
    source: '官方申请', note: '', expires_on: '2026-10-12', use_in_proxy: false,
    last_status: 'ok', last_check: '11-11 19:00', models: 118, has_plain: true,
  }],
  masked: [{
    site: 'p-ollama', site_name: '本地 Ollama', site_kind: 'local',
    homepage: '', base_url: 'http://127.0.0.1:11434/v1',
    id: 'k3', label: '本地', masked: '••••',
    source: '', note: '', expires_on: '', use_in_proxy: false,
    last_status: null, last_check: null, models: 3, has_plain: false,
  }],
  partial: [{
    site: 'p-bailian', site_name: '阿里云百炼', site_kind: 'official',
    homepage: 'https://bailian.console.aliyun.com',
    base_url: 'https://dashscope.aliyuncs.com/compatible-mode/v1',
    id: 'k1', label: '主用', masked: 'sk-ws9••••3fA2',
    source: '群里分享', note: '备注一行', expires_on: '2026-12-31',
    use_in_proxy: true, last_status: 'ok', last_check: '11-10 10:00', models: 42, has_plain: true,
  }],
  total: 3,
};

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

    // ① 面板与控件都在
    assert.equal(await page.locator('#key-find').isVisible(), true, '密钥管理页应有「找密钥」面板');
    assert.equal(await page.locator('#k-find-q').isVisible(), true);
    assert.equal(await page.locator('#k-find-go').isVisible(), true);

    // ② 三档命中分开显示，且每条都带「位置」
    const sent = [];
    await page.exposeFunction('__record', (p) => sent.push(p));
    await page.evaluate((fake) => {
      const orig = invoke;
      invoke = async (cmd, arg) => {
        if (cmd === 'action' && arg && arg.payload && arg.payload.action === 'key_search') {
          window.__record(arg.payload);
          return fake;
        }
        return orig(cmd, arg);
      };
    }, FAKE);

    await page.locator('#k-find-q').fill('sk-testfull123456');
    await page.locator('#k-find-go').click();
    await page.waitForSelector('#k-find-out .kfrow');
    assert.equal(sent.length, 1, '应只发一次 key_search');
    assert.equal(sent[0].query, 'sk-testfull123456');

    const rows = page.locator('#k-find-out .kfrow');
    assert.equal(await rows.count(), 3, '三档共 3 条');
    assert.equal(await page.locator('#k-find-out .kfrow.exact').count(), 1, '完全一致 1 条');
    assert.equal(await page.locator('#k-find-out .kfrow.masked').count(), 1, '打码一致 1 条');
    assert.equal(await page.locator('#k-find-out .kfrow.partial').count(), 1, '片段匹配 1 条');

    // 每条都要说清在哪个站点的哪一条
    const txt = await page.locator('#k-find-out').innerText();
    assert.match(txt, /某中转站 A/, '应显示站点名');
    assert.match(txt, /备用/, '应显示密钥名称');
    assert.match(txt, /位置/, '应有位置说明');
    await page.screenshot({ path: path.join(evidence, 'hits.png') });

    // ④ 「在台账里定位」：展开站点并把目标行点亮
    await page.locator('#k-find-out .kfrow.exact button', { hasText: '在台账里定位' }).click();
    await page.waitForTimeout(300);
    const hitRows = page.locator('tr.hit');
    assert.ok(await hitRows.count() > 0, '定位后应有行被点亮');
    await page.screenshot({ path: path.join(evidence, 'locate.png') });

    // ③ 查不到：如实说明，不是空白
    await page.evaluate((fake) => {
      const orig = invoke;
      invoke = async (cmd, arg) => {
        if (cmd === 'action' && arg && arg.payload && arg.payload.action === 'key_search') {
          return { ok: true, locked: false, query: 'x', exact: [], masked: [], partial: [], total: 0 };
        }
        return orig(cmd, arg);
      };
    }, FAKE);
    await page.locator('#k-find-q').fill('sk-nothing-here');
    await page.locator('#k-find-go').click();
    await page.waitForTimeout(200);
    const missTxt = await page.locator('#k-find-out').innerText();
    assert.match(missTxt, /没找到/, '查不到要如实说');
    await page.screenshot({ path: path.join(evidence, 'miss.png') });

    assert.equal(errors.length, 0, `页面报错：${errors.join(' / ')}`);
    console.log('ok 找密钥：面板 / 三档展示 / 位置说明 / 定位高亮 / 查不到如实提示');
  } finally {
    if (browser) await browser.close();
    server.close();
  }
})().catch((e) => { console.error(e); process.exit(1); });
