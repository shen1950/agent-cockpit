/* 福利活动列表的行布局与「编辑」切换。
   三条底线：① 右边的时间（还剩 N 天 / 每天）永远是横着的一行、显示全；
   ② 内容行固定在标题下面一行、左边对齐，不许省略号截断；③ 表单开着时点另一条的「编辑」必须能切过去。 */
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const { chromium } = require('D:/npm-global/node_modules/@playwright/mcp/node_modules/playwright');

const root = path.resolve(__dirname, '../ui');
const mime = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.svg': 'image/svg+xml', '.png': 'image/png' };
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

const PERKS = [
  { id: 'p1', app: 'qoder-cn', app_name: 'Qoder CN', title: 'Deepseek-V4.1-flash 0×', detail: '', ends_on: '2026-10-31', daily: false, url: '', live: true, days_left: 4 },
  { id: 'p2', app: 'qoder-cn', app_name: 'Qoder CN', title: '每天免费用 Qwen3.8-Flash', detail: '官方公告至今没给结束日期（另有 09:00/15:00 的监控任务盯着）', ends_on: '', daily: true, url: '', live: true, days_left: null },
  { id: 'p3', app: 'workbuddy-intl', app_name: 'WorkBuddy 国际版', title: 'Deepseek-V4.1-flash 0×', detail: '', ends_on: '', daily: true, url: '', live: true, days_left: null },
  { id: 'p4', app: '', app_name: '通用', title: 'DeepSeek-V4 flash 倍率 0', detail: '按倍率计费的产品里这一档等于免费用', ends_on: '', daily: true, url: '', live: true, days_left: null },
];

async function seed(page) {
  await page.evaluate((perks) => {
    S.view = 'agents';
    S.cur = 'qoder-cn';
    S.data.perks = perks;
    render();
  }, PERKS);
}

function boxOf(page, sel) {
  return page.evaluate((s) => [...document.querySelectorAll(s)].map((n) => {
    const r = n.getBoundingClientRect();
    return {
      text: (n.textContent || '').trim(),
      w: Math.round(r.width), h: Math.round(r.height),
      scrollW: n.scrollWidth, clientW: n.clientWidth,
    };
  }), sel);
}

(async () => {
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  const base = `http://127.0.0.1:${server.address().port}/index.html`;
  const browser = await chromium.launch({ channel: 'chrome', headless: true });
  const page = await browser.newPage({ viewport: { width: 1080, height: 900 } });
  const fails = [];
  const check = (ok, msg) => { if (!ok) fails.push(msg); };

  for (const width of [1080, 860, 720]) {
    await page.setViewportSize({ width, height: 900 });
    await page.goto(base, { waitUntil: 'load' });
    await seed(page);
    await page.waitForSelector('#perks-list .perk');

    const rows = await boxOf(page, '#perks-list .perk');
    check(rows.length === 4, `[${width}px] 应有 4 行，实际 ${rows.length}`);

    // ① 时间：单行、不溢出（竖排时宽度只有十几 px、高度会翻倍）
    const exps = await boxOf(page, '#perks-list .perk .exp');
    check(exps.length === 4, `[${width}px] 应有 4 个时间标签，实际 ${exps.length}`);
    for (const e of exps) {
      check(e.h <= 24, `[${width}px] 时间「${e.text}」被压成了 ${e.h}px 高（竖排）`);
      // 「每天」两个 13px 汉字就是 26px；竖排时高度会翻倍、宽度只剩一个字
      check(e.w >= 24, `[${width}px] 时间「${e.text}」只有 ${e.w}px 宽（被挤扁）`);
      check(e.scrollW <= e.clientW + 1, `[${width}px] 时间「${e.text}」被截断（${e.scrollW}>${e.clientW}）`);
    }

    // ② 内容行：永远在标题下面一行、左边跟标题对齐，换行而不是省略号
    const pds = await boxOf(page, '#perks-list .perk .pd');
    check(pds.length === 2, `[${width}px] 应有 2 条带内容的福利，实际 ${pds.length}`);
    for (const d of pds) {
      check(d.scrollW <= d.clientW + 1, `[${width}px] 内容「${d.text.slice(0, 12)}…」仍被截断`);
      check(d.w >= 70, `[${width}px] 内容「${d.text.slice(0, 12)}…」被压到 ${d.w}px`);
    }
    const geo = await page.evaluate(() => [...document.querySelectorAll('#perks-list .perk')].map((row) => {
      const b = row.querySelector('b');
      const pd = row.querySelector('.pd');
      if (!pd) return null;
      const rb = b.getBoundingClientRect();
      const rp = pd.getBoundingClientRect();
      return {
        title: b.textContent, dLeft: Math.round(rp.left - rb.left), dTop: Math.round(rp.top - rb.top),
        bBottom: Math.round(rb.bottom), titleClipped: b.scrollWidth > b.clientWidth + 1,
      };
    }).filter(Boolean));
    for (const g of geo) {
      check(Math.abs(g.dLeft) <= 1, `[${width}px] 内容没和标题左对齐（差 ${g.dLeft}px）：${g.title}`);
      check(g.dTop >= 0, `[${width}px] 内容跑到标题上面去了：${g.title}`);
      check(!g.titleClipped, `[${width}px] 标题被省略号截断：${g.title}`);
    }
    // 有内容的行必须比只有标题的行高（内容真的占了第二行）
    check(rows[1].h > rows[0].h, `[${width}px] 带内容的行没变高（${rows[1].h} vs ${rows[0].h}）`);
    const pdWrap = await page.evaluate(() => getComputedStyle(document.querySelector('#perks-list .perk .pd')).textOverflow);
    check(pdWrap !== 'ellipsis', `[${width}px] 内容还是 text-overflow:ellipsis`);

    // ③ 编辑：先开「添加福利」表单，再去点某一条的「编辑」，必须切过去
    await page.locator('#perks-list button', { hasText: '添加福利' }).first().click();
    await page.waitForSelector('.perkform');
    const emptyTitle = await page.inputValue('.perkform input[placeholder*="Qwen3.8-Max"]');
    check(emptyTitle === '', `[${width}px] 添加表单标题应为空，实际「${emptyTitle}」`);

    await page.locator('#perks-list .perk').nth(1).locator('button', { hasText: '编辑' }).click();
    await page.waitForSelector('.perkform');
    const t2 = await page.inputValue('.perkform input[placeholder*="Qwen3.8-Max"]');
    check(t2 === '每天免费用 Qwen3.8-Flash', `[${width}px] 点第 2 条「编辑」表单没切过去（标题「${t2}」）`);
    const d2 = await page.inputValue('.perkform input[placeholder*="0 倍率"]');
    check(d2.startsWith('官方公告至今'), `[${width}px] 编辑表单没带上内容（「${d2}」）`);

    // 表单开着再点第 4 条：切换 + 程序回到"通用"
    await page.locator('#perks-list .perk').nth(3).locator('button', { hasText: '编辑' }).click();
    const t4 = await page.inputValue('.perkform input[placeholder*="Qwen3.8-Max"]');
    check(t4 === 'DeepSeek-V4 flash 倍率 0', `[${width}px] 从第 2 条切到第 4 条失败（标题「${t4}」）`);
    const app4 = await page.inputValue('.perkform select');
    check(app4 === '', `[${width}px] 通用福利的程序应为空，实际「${app4}」`);

    // ④ 勾选框尺寸正常（不该被 input{min-width:120px} 撑开）
    const cb = await boxOf(page, '.perkform input[type="checkbox"]');
    check(cb.length === 1 && cb[0].w <= 22, `[${width}px] 勾选框宽 ${cb.length ? cb[0].w : '缺失'}px，被撑开了`);
    // 勾选框和「每天重复」两个字之间不该隔一大片空白
    const gap = await page.evaluate(() => {
      const c = document.querySelector('.perkform input[type="checkbox"]');
      const l = c.closest('label');
      const t = l.lastChild;
      const range = document.createRange();
      range.selectNodeContents(t);
      const r = range.getBoundingClientRect();
      return Math.round(r.left - c.getBoundingClientRect().right);
    });
    check(gap <= 10, `[${width}px] 勾选框与「每天重复」间隔 ${gap}px`);

    await page.evaluate(() => { S.perkForm = null; render(); });
  }

  await browser.close();
  server.close();
  if (fails.length) {
    console.error('perk layout FAIL\n' + fails.map((f) => '  - ' + f).join('\n'));
    process.exit(1);
  }
  console.log('perk layout OK（1080 / 860 / 720 三档宽度 × 时间不竖排 × 备注换行 × 编辑可切换 × 勾选框尺寸）');
})();
