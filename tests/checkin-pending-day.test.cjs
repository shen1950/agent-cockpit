// 「正在等到账」按活跃日区分：同一个号欠着昨天和今天两笔时，两行必须长得不一样
// （2026-11-06 她圈出来的：两行都写"今天这份积分还没到账"，看不出在等哪天）
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const { chromium } = require('playwright');
const root = path.resolve(__dirname, '../ui');
const server = http.createServer((req, res) => {
  const p = path.resolve(root, '.' + (req.url === '/' ? '/index.html' : req.url.split('?')[0]));
  if (!p.startsWith(root + path.sep)) return res.writeHead(403).end();
  fs.readFile(p, (e, data) => {
    if (e) return res.writeHead(404).end();
    const type = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.svg': 'image/svg+xml' }[path.extname(p)];
    res.setHeader('Content-Type', type || 'application/octet-stream'); res.end(data);
  });
});

const day = (offset) => {
  const d = new Date();
  d.setDate(d.getDate() - offset);
  const p2 = (n) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p2(d.getMonth() + 1)}-${p2(d.getDate())}`;
};
const cnDate = (s) => `${Number(s.slice(5, 7))}月${Number(s.slice(8, 10))}日`;

(async () => {
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  try {
    const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
    await page.goto(`http://127.0.0.1:${server.address().port}/`);
    await page.waitForFunction(() => typeof S !== 'undefined' && S.data);

    const y = day(1), t = day(0), d4 = day(4);
    await page.evaluate(({ y, t }) => {
      clearInterval(pollTimer);
      invoke = () => new Promise(() => {});
      // 先 render 再塞数据：render() 会用 mock 的 state 报文覆盖 S.checkin，反过来会被画回假数据
      S.view = 'checkin'; render();
      const row = { product: 'workbuddy-intl', app: 'workbuddy-intl', today: t, scope: 'product',
        name: 'WorkBuddy 国际版', supported: true, activity_on: true, signed_today: true, claimable: false };
      S.checkin = { ok: true, fetched_at: Math.floor(Date.now() / 1000), items: [row],
        state: { log: [], pending: {
          [`workbuddy-intl@Gerald Fabinhu|${t}`]: { today: t, label: 'WorkBuddy 国际版 · Gerald Fabinhu', expect: '今天这份积分' },
          [`workbuddy-intl@Gerald Fabinhu|${y}`]: { today: y, label: 'WorkBuddy 国际版 · Gerald Fabinhu', expect: '今天这份积分' },
          [`workbuddy-intl@shen1950|${t}`]: { today: t, label: 'WorkBuddy 国际版 · shen1950', expect: '今天这份积分' } } } };
      renderCheckin();
    }, { y, t });

    const texts = await page.locator('.cipend .cihit').allTextContents();
    assert.equal(texts.length, 3, `三笔都该画出来：${JSON.stringify(texts)}`);
    assert.notEqual(texts[0], texts[1], '同账号跨天的两行不许长一样');
    assert.match(texts[0], /昨天这份积分还没到账/, '等得最久的（昨天）排最前');
    assert.match(texts[1], /今天这份积分还没到账/);
    assert.match(texts[2], /shen1950：今天这份积分还没到账/);

    const out = path.resolve(__dirname, '../.agent-sync/checkin-pending-evidence');
    fs.mkdirSync(out, { recursive: true });
    await page.locator('.cipend').screenshot({ path: path.join(out, 'pending-two-days.png') });

    // 厂商短语带数量时要留住："今天这 30 积分" → "昨天这 30 积分"，不是"昨天这份积分"
    await page.evaluate(({ y }) => {
      S.checkin.state.pending = { 'k1': { today: y, label: 'WorkBuddy 国际版', expect: '今天这 30 积分' } };
      renderCheckin();
    }, { y });
    assert.equal(await page.locator('.cipend .cihit').first().textContent(),
      'WorkBuddy 国际版：昨天这 30 积分还没到账');

    // expect 里没有日期词时补一个"的"，别写出"昨天奖励积分"这种半截话
    await page.evaluate(({ y }) => {
      S.checkin.state.pending = { 'k2': { today: y, label: 'WorkBuddy 国际版', expect: '奖励积分' } };
      renderCheckin();
    }, { y });
    assert.equal(await page.locator('.cipend .cihit').first().textContent(),
      'WorkBuddy 国际版：昨天的奖励积分还没到账');

    // 老数据没有 today 字段：退回原样，不许写成"的这份积分"
    await page.evaluate(() => {
      S.checkin.state.pending = { 'k3': { label: 'WorkBuddy 国际版 · Gerald Fabinhu', expect: '今天这份积分' } };
      renderCheckin();
    });
    assert.equal(await page.locator('.cipend .cihit').first().textContent(),
      'WorkBuddy 国际版 · Gerald Fabinhu：今天这份积分还没到账');

    // 超过前天：报具体日期，不写"大前天"这种没完的相对词
    await page.evaluate(({ d4 }) => {
      S.checkin.state.pending = { 'k4': { today: d4, label: 'WorkBuddy 国际版', expect: '今天这份积分' } };
      renderCheckin();
    }, { d4 });
    assert.equal(await page.locator('.cipend .cihit').first().textContent(),
      `WorkBuddy 国际版：${cnDate(d4)}这份积分还没到账`);

    // 到期没等来的：从「正在等」里摘出去，但要留下欠的是哪天、什么时候收的手
    const DEADLINE = 1794442074, DROPPED = 1794528474;
    await page.evaluate(({ y, d4, DEADLINE, DROPPED }) => {
      S.checkin.state.pending = { k: { today: y, label: 'WorkBuddy 国际版 · Gerald Fabinhu', expect: '今天这份积分' } };
      S.checkin.state.expired = [
        { key: `workbuddy-intl@Gerald Fabinhu|${d4}`, today: d4, label: 'WorkBuddy 国际版 · Gerald Fabinhu',
          expect: '今天这份积分', deadline: DEADLINE, dropped_at: DROPPED },
        { key: 'old', today: '2026-01-02', label: 'WorkBuddy 国际版 · 老号', expect: '今天这份积分',
          deadline: DEADLINE - 86400, dropped_at: DROPPED - 86400 },
      ];
      renderCheckin();
    }, { y, d4, DEADLINE, DROPPED });
    const pendRows = await page.locator('.cipend:not(.past) .cihit').allTextContents();
    const pastRows = await page.locator('.cipend.past .cihit').allTextContents();
    assert.equal(pendRows.length, 1, '到期的不许还挂在「正在等」里');
    assert.equal(pastRows.length, 2);
    assert.match(pastRows[0], /WorkBuddy 国际版 · Gerald Fabinhu：\d+月\d+日这份积分至今未到账（盯到 \d\d-\d\d \d\d:\d\d）/,
      `最新那条该写清哪天欠的、什么时候收手：${pastRows[0]}`);
    assert.match(await page.locator('.cipend.past b').textContent(), /已停止追踪/);
    await page.locator('.cipend.past').screenshot({ path: path.join(out, 'pending-expired.png') });

    // 超过 10 条只摆最近 10 条，剩下的一句带过，别把签到页顶成流水账
    await page.evaluate(({ DEADLINE, DROPPED }) => {
      S.checkin.state.expired = Array.from({ length: 14 }, (_, i) => ({
        key: 'k' + i, today: '2026-11-01', label: 'L' + i, expect: '今天这份积分',
        deadline: DEADLINE, dropped_at: DROPPED + i,
      }));
      renderCheckin();
    }, { DEADLINE, DROPPED });
    const many = await page.locator('.cipend.past .cihit').allTextContents();
    assert.equal(many.length, 11, `10 条 + 一句「另有 N 条」，实际 ${many.length}`);
    assert.equal(many[0], 'L13：11月1日这份积分至今未到账（盯到 11-12 08:07）', '按收手时间从新到旧排');
    assert.equal(many[10], '另有 4 条更早的');

    console.log('pending-day: 跨天可区分、最久排前、四种 expect 形状、到期留痕且只摆最近 10 条。');
  } finally { await browser.close(); server.close(); }
})().catch((e) => { console.error(e); server.close(); process.exitCode = 1; });
