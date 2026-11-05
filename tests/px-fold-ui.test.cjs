/* 用本机预览桥（mock.js）真渲染一遍，确认：
   ① 每个 Agent 页的「模型反代」面板真的没了，只剩「接入本地网关」，且标题字体与老的
      ·  .panelhead h2 完全一致（同一个字号/字重/字族）
   ② 「模型反代」总页的 反代全景 / 供应商 两块都能收起-展开，状态记进 localStorage
   截图落在 .agent-sync/px-fold-evidence/。 */
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const { chromium } = require('playwright');

const root = path.resolve(__dirname, '../ui');
const evidence = path.resolve(__dirname, '../.agent-sync/px-fold-evidence');
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

    /* ---------- ① Agent 页：模型反代面板没了，接入本地网关顶上 ---------- */
    await page.evaluate(() => { S.view = 'agents'; S.cur = 'workbuddy'; render(); });
    await page.waitForSelector('#gateway-client h2', { timeout: 15000 });
    await page.evaluate(() => document.fonts.ready);
    const agentPage = await page.evaluate(() => {
      const pick = (sel) => {
        const el = document.querySelector(sel);
        if (!el) return null;
        const cs = getComputedStyle(el);
        return { text: el.textContent.trim(), size: cs.fontSize, weight: cs.fontWeight, family: cs.fontFamily };
      };
      return {
        oldPanel: !!document.querySelector('#agent-proxy-panel') || !!document.querySelector('#agent-proxy'),
        title: pick('#gateway-client h2'),
        sub: pick('#gateway-client .sub'),
        // 同一页上另一块老 panel 的标题，当作字体基准
        ref: pick('#view-agents .panelhead h2'),
        chip: pick('#gateway-client header .chip'),
      };
    });
    assert.equal(agentPage.oldPanel, false, '「模型反代」面板必须已经删掉');
    assert.equal(agentPage.title.text, '接入本地网关');
    assert.ok(agentPage.sub && agentPage.sub.text, '标题旁要有说明小字');
    assert.ok(agentPage.ref, '找不到基准标题');
    assert.deepEqual(
      { size: agentPage.title.size, weight: agentPage.title.weight, family: agentPage.title.family },
      { size: agentPage.ref.size, weight: agentPage.ref.weight, family: agentPage.ref.family },
      `接入本地网关的字体要与原「模型反代」一致：${JSON.stringify(agentPage)}`,
    );
    await page.screenshot({ path: path.join(evidence, 'agent-gateway.png'), fullPage: true });
    // 面板本身单独来一张：标题字体与 chip 的位置只看这一张最清楚
    await page.locator('#gateway-client').screenshot({ path: path.join(evidence, 'agent-gateway-panel.png') });

    /* ---------- ② 总页：三块的分节收起 / 展开 ---------- */
    await page.evaluate(() => { S.view = 'proxy'; render(); });
    await page.waitForSelector('#proxy-models .px-head');
    await page.evaluate(() => loadProxyModels());
    await page.waitForSelector('#proxy-models .px-mcard', { timeout: 20000 });

    // 反代全景把类挂在自己的容器上；供应商/接入密钥挂在内层 section 上
    const foldState = async (container) => page.evaluate((sel) => {
      const host = document.querySelector(sel);
      if (!host) return null;
      const marked = host.classList.contains('px-folded') ? host : host.querySelector('.px-folded');
      const root = marked || host.querySelector(':scope > section') || host;
      const head = root.querySelector('.px-head');
      const body = head ? [...root.children].filter((n) => n !== head) : [];
      return {
        foldedClass: !!marked,
        visibleBody: body.filter((n) => getComputedStyle(n).display !== 'none').length,
        buttons: head ? [...head.querySelectorAll('button')].map((b) => b.textContent.trim()) : [],
      };
    }, container);

    // 反代全景
    let st = await foldState('#proxy-models');
    assert.equal(st.foldedClass, false, '默认展开');
    assert.ok(st.visibleBody > 0);
    assert.ok(st.buttons.includes('收起'), `标题行要有收起按钮：${st.buttons}`);
    await page.click('#proxy-models .px-head >> text=收起');
    st = await foldState('#proxy-models');
    assert.equal(st.foldedClass, true, '点收起后整块要收起来');
    assert.equal(st.visibleBody, 0, '收起后正文不该再占高度');
    assert.ok(st.buttons.includes('展开'));
    const saved = await page.evaluate(() => JSON.parse(localStorage.getItem('cockpit-proxy-fold') || '{}'));
    assert.equal(saved.models, true, '收起状态要记进 localStorage');
    await page.screenshot({ path: path.join(evidence, 'proxy-models-folded.png'), fullPage: true });
    await page.click('#proxy-models .px-head >> text=展开');
    st = await foldState('#proxy-models');
    assert.equal(st.foldedClass, false);
    assert.ok(st.visibleBody > 0);

    // 供应商
    await page.click('#proxy-suppliers .gateway-supplier-head button:has-text("收起")');
    await page.waitForTimeout(100);
    st = await foldState('#proxy-suppliers');
    assert.equal(st.foldedClass, true, '供应商要能收起');
    assert.equal(st.visibleBody, 0);
    await page.click('#proxy-suppliers .gateway-supplier-head button:has-text("展开")');
    assert.equal((await foldState('#proxy-suppliers')).foldedClass, false);


    // 分组级收起：反代全景里每个 Agent 一组
    const groupHead = await page.evaluate(() => {
      const heads = [...document.querySelectorAll('#proxy-models .px-modelgroup-head')];
      return heads.length ? heads[0].querySelector('button:last-child').textContent.trim() : '';
    });
    assert.equal(groupHead, '收起', '每个 Agent 分组也要能收起');
    await page.evaluate(() => {
      const head = document.querySelector('#proxy-models .px-modelgroup-head');
      head.querySelector('button:last-child').click();
    });
    const gridHidden = await page.evaluate(() => {
      const grid = document.querySelector('#proxy-models .px-mgrid');
      return !grid || getComputedStyle(grid).display === 'none';
    });
    assert.equal(gridHidden, true, '分组收起后它的模型卡片要隐藏');

    const isFolded = (sel) => page.evaluate((s) => {
      const host = document.querySelector(s);
      return !!host && (host.classList.contains('px-folded') || !!host.querySelector('.px-folded'));
    }, sel);

    // 记住状态：重开页面仍按上次的样子
    await page.reload({ waitUntil: 'networkidle' });
    await page.waitForFunction(() => typeof S !== 'undefined' && S.data && !S.busy);
    await page.evaluate(() => { S.view = 'proxy'; render(); });
    await page.waitForSelector('#proxy-models .px-head');
    assert.equal(await isFolded('#proxy-models'), false);

    // 左栏点供应商入口：入口只能滚动定位，不改变供应商自己的展开状态
    await page.evaluate(() => localStorage.setItem('cockpit-proxy-fold', JSON.stringify({ suppliers: false })));
    await page.reload({ waitUntil: 'networkidle' });
    await page.waitForFunction(() => typeof S !== 'undefined' && S.data && !S.busy);
    await page.evaluate(() => { S.view = 'proxy'; render(); });
    await page.waitForSelector('#proxy-suppliers .gateway-supplier-head');
    assert.equal(await isFolded('#proxy-suppliers'), false);
    await page.click('#proxy-nav [data-sec="suppliers"]');
    await page.waitForTimeout(200);
    assert.equal(await isFolded('#proxy-suppliers'), false, '左栏入口应保持展开状态');

    assert.deepEqual(errors, [], '页面不该有 JS 报错');
    console.log(JSON.stringify({ result: 'PASS', evidence }));
  } finally {
    if (browser) await browser.close();
    await new Promise((r) => server.close(r));
  }
})().catch((error) => { console.error(error); process.exitCode = 1; });
