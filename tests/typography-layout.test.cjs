/* Actual browser fonts and menu alignment. Uses only the local preview bridge. */
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const { chromium } = require('D:/npm-global/node_modules/@playwright/mcp/node_modules/playwright');
const root = path.resolve(__dirname, '../ui');
const artifacts = path.resolve(__dirname, '../.agent-sync/typography-evidence');
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
async function seed(page, appearance) {
  await page.evaluate(({ theme, scale, variant = 'chatgpt' }) => {
    clearInterval(pollTimer);
    const source = structuredClone(window.typographySource ||= S.data.instances[0]);
    const timestamp = Math.floor(Date.now() / 1000);
    const name = variant === 'zcode' ? 'Irshad Taj' : variant === 'api' ? 'nick9994374911' : 'anna.smith-demo-85f935@example.test';
    const account = Object.assign(source, {
      app: 'codex-desktop', app_name: 'ChatGPT', name, kind: 'pwa',
      running: true, resident: false, procs: 8, created: '2026-10-27',
      zcode_auth: null,
      data_dir: 'C:\\Users\\Example\\.agent-cockpit\\instances\\codex-desktop\\account-demo',
      chatgpt_auth: { has_auth: true, mode: 'chatgpt', native: false, account: name,
        home: 'C:\\Users\\Example\\.agent-cockpit\\chatgpt-auth\\account-demo',
        history_snapshot: { threads: 131, projects: 12, visibility_version: 2 } },
      chatgpt_quota: { state: 'live', mode: 'chatgpt', fetched_at: timestamp, balance: null,
        windows: [{ id: '5h', label: '5 小时', remaining_percent: 60, used_percent: 40, reset_at: timestamp + 1800 },
          { id: 'weekly', label: '每周', remaining_percent: 47, used_percent: 53, reset_at: timestamp + 86400 }] },
    });
    S.data.apps = [{ id: 'codex-desktop', name: 'ChatGPT', short: 'CG', kind: 'pwa',
      installed: true, isolatable: true, swap_ok: false, icon: '', swapped: '' }];
    if (variant === 'api') {
      account.chatgpt_auth.mode = 'apikey';
      account.chatgpt_quota = { state: 'live', mode: 'apikey', fetched_at: timestamp, windows: [],
        balance: { remaining: 1732.897, total: 8000, used: 6267.103, currency: 'USD' } };
    } else if (variant === 'zcode') {
      Object.assign(account, {
        app: 'zcode', app_name: 'ZCode', kind: 'electron', logged_in: true,
        in_place: true, released: false, swapped: false,
        chatgpt_auth: null, chatgpt_quota: null, data_dir: 'C:\\Users\\Example\\.zcode',
        zcode_auth: { provider: 'zai', account: name, user_id: 'zcode:zai:demo', has_auth: true,
          current: true, auth_status: 'ready', quota_state: 'ready', native_data_dir: 'C:\\Users\\Example\\.zcode' },
        quota: { ...source.quota, total: 8000000, left: 1732897, unit: 'token', stale: false,
          live_error: '', fetched_at: timestamp, expires_on: '2026-10-28', days_left: 1,
          groups: [], credits: [{ label: '平台奖励积分', category: '平台奖励积分', unit: 'token',
            left: 1732897, total: 8000000, used: 6267103, expires_on: '2026-10-28', days_left: 1 }] },
      });
      S.data.apps = [{ id: 'zcode', name: 'ZCode', short: 'ZC', kind: 'electron', installed: true,
        isolatable: false, shared_profile: true, auth_switch: true, local_quota: true,
        swap_ok: false, icon: '', machine: { known: true, value: { ok: true, account: name, user_id: 'zcode:zai:demo' } } }];
    }
    S.data.instances = [account];
    S.data.settings.appearance = { ...S.data.settings.appearance, theme, font_scale: scale };
    const fixture = S.data;
    invoke = async (command, args = {}) => {
      if (command === 'get_state') return fixture;
      if (command === 'get_meta') return account;
      if (args.payload?.action === 'chatgpt_status') return { ok: true, status: account.chatgpt_auth };
      return { ok: true, msg: 'Preview' };
    };
    S.cur = account.app; S.view = 'agents'; S.busy = false;
    render();
  }, appearance);
}
async function inspect(page) {
  return page.evaluate(() => {
    const problems = [];
    const nav = document.getElementById('im-nav');
    const body = document.getElementById('im-body');
    const title = document.getElementById('im-title');
    const navRect = nav.getBoundingClientRect();
    if (navRect.width > 145) problems.push('excessive navigation width');
    if (navRect.right > body.getBoundingClientRect().left) problems.push('navigation overlaps content');
    if (body.scrollWidth > body.clientWidth + 2) problems.push('content overflows horizontally');
    for (const button of nav.querySelectorAll('button')) {
      const box = button.getBoundingClientRect();
      const range = document.createRange(); range.selectNodeContents(button);
      const text = range.getBoundingClientRect();
      const style = getComputedStyle(button);
      if (Math.abs((text.left + text.right - box.left - box.right) / 2) > 1.5) problems.push(`uncentered menu ${button.textContent}`);
      if (style.borderTopWidth !== '0px') problems.push(`unnecessary border ${button.textContent}`);
      if (style.fontWeight > 600) problems.push(`heavy menu font ${button.textContent}`);
      if (button.getAttribute('aria-current') === 'true') {
        const probe = document.createElement('span'); probe.style.color = 'var(--accent)'; document.body.appendChild(probe);
        if (style.color !== getComputedStyle(probe).color) problems.push('active menu is outside the accent palette');
        probe.remove();
      }
    }
    for (const node of document.querySelectorAll('.cpt-quota-line b,.cpt-quota-time,.dlgsub,.account-email,.card .nm,.card .swapmark,.qused')) {
      if (/Cascadia|Consolas|Courier|Georgia|Times New Roman/i.test(getComputedStyle(node).fontFamily)) problems.push(`wrong data font ${node.className}`);
    }
    const summary = body.querySelector('.cpt-run-summary');
    if (summary) {
      const open = summary.querySelector('.btn');
      if (!open) problems.push('run header lacks primary action');
      if (body.querySelectorAll('.cpt-run-summary .btn').length !== 1) problems.push('floating run actions');
      if ([...body.querySelectorAll('button')].some(b => b.textContent === '刷新状态')) problems.push('redundant run refresh');
    }
    return { problems, titleFamily: getComputedStyle(title).fontFamily, navWidth: navRect.width,
      bodyWidth: body.clientWidth, activeMenu: nav.querySelector('[aria-current="true"]')?.textContent };
  });
}
async function actualFont(cdp, selector, expected) {
  const { root: documentNode } = await cdp.send('DOM.getDocument');
  const { nodeId } = await cdp.send('DOM.querySelector', { nodeId: documentNode.nodeId, selector });
  assert.ok(nodeId, `font sample is missing: ${selector}`);
  const { fonts } = await cdp.send('CSS.getPlatformFontsForNode', { nodeId });
  const used = fonts.filter(font => font.glyphCount > 0);
  assert.ok(used.length, `font sample has no rendered glyphs: ${selector}`);
  const serif = /Georgia|Times New Roman/i;
  const mono = /Cascadia|Source Code Pro|Consolas|Courier/i;
  const found = expected === 'serif' ? used.some(font => serif.test(font.familyName))
    : expected === 'mono' ? used.some(font => mono.test(font.familyName))
      : used.every(font => !/Georgia|Times New Roman|Noto Serif|SimSun|Songti|Cascadia|Source Code Pro|Consolas|Courier/i.test(font.familyName));
  assert.ok(found, JSON.stringify({ selector, expected, actual: used }));
  return { selector, expected, fonts: used };
}
async function inspectHome(page, cdp) {
  await page.evaluate(() => {
    S.data.stats = { ...S.data.stats, agents: 21, instances: 20 };
    S.data.alerts = Array.from({ length: 16 }, (_, index) => ({ app_id: 'codex-desktop',
      app: 'ChatGPT', name: `preview-${index}`, label: '积分', left: 100, unit: 'credits', days: 14, level: 'warn' }));
    S.data.checkin = { ok: true, fetched_at: Math.floor(Date.now() / 1000),
      items: Array.from({ length: 11 }, (_, index) => ({ product: `preview-${index}`, name: `签到 ${index + 1}`,
        signed_today: true, claimable: false })), state: { log: [] } };
    S.view = 'home'; render();
  });
  await page.evaluate(() => document.fonts.ready);
  const report = await page.evaluate(() => {
    const stats = [...document.querySelectorAll('#home-body .hstat-v')];
    const host = document.getElementById('home-body');
    return { values: stats.map(node => node.textContent), problems: stats.flatMap(node => {
      const box = node.getBoundingClientRect();
      const tile = node.closest('.hstat').getBoundingClientRect();
      return box.width > 0 && box.left >= tile.left && box.right <= tile.right + 1 ? [] : ['home statistic exceeds its tile'];
    }).concat(host.scrollWidth > host.clientWidth + 2 ? ['home content overflows horizontally'] : []) };
  });
  assert.deepEqual(report.values, ['21', '20', '16', '11/11']);
  assert.deepEqual(report.problems, []);
  const actualFonts = [await actualFont(cdp, '.brand .name', 'serif')];
  for (let index = 1; index <= report.values.length; index++) {
    actualFonts.push(await actualFont(cdp, `#home-body .hstat:nth-child(${index}) .hstat-v`, 'sans'));
  }
  return { ...report, actualFonts };
}
(async () => {
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  fs.mkdirSync(artifacts, { recursive: true });
  let browser;
  try {
    browser = await chromium.launch({ channel: 'chrome', headless: true });
    const page = await browser.newPage();
    const errors = []; page.on('pageerror', error => errors.push(error.message));
    await page.addInitScript(() => localStorage.setItem('cockpit-toured', '1'));
    await page.goto(`http://127.0.0.1:${server.address().port}/`, { waitUntil: 'networkidle' });
    await page.waitForFunction(() => typeof S !== 'undefined' && S.data && !S.busy);
    const cdp = await page.context().newCDPSession(page);
    await cdp.send('DOM.enable'); await cdp.send('CSS.enable');
    const reports = [];
    for (const theme of ['light', 'dark']) {
      for (const [width, height, scale] of [[1320, 920, 100], [960, 850, 125]]) {
        await page.setViewportSize({ width, height });
        for (const variant of ['chatgpt', 'api', 'zcode']) {
          await seed(page, { theme, scale, variant });
          for (const section of variant === 'chatgpt' ? ['quota', 'run'] : ['quota']) {
            await page.evaluate(async (sec) => openDetail(S.cur, S.data.instances[0].name, sec), section);
            await page.evaluate(() => document.fonts.ready);
            const report = await inspect(page);
            assert.deepEqual(report.problems, [], JSON.stringify({ theme, width, variant, section, ...report }));
            const actualFonts = [];
            for (const selector of ['.brand .name', '#cur-title', '#im-title']) actualFonts.push(await actualFont(cdp, selector, 'serif'));
            actualFonts.push(await actualFont(cdp, '.dlgsub', 'sans'));
            if (variant === 'chatgpt') {
              actualFonts.push(await actualFont(cdp, '#im-title .account-email', 'sans'));
              actualFonts.push(await actualFont(cdp, '.card .account-email', 'sans'));
              actualFonts.push(await actualFont(cdp, '.cpt-quota-line b', 'sans'));
            } else {
              actualFonts.push(await actualFont(cdp, '.card .nm', 'sans'));
              const values = variant === 'zcode' ? ['.quota .val', '.qbig b', '.qcat b'] : ['.cpt-balance strong'];
              for (const selector of values) actualFonts.push(await actualFont(cdp, selector, 'mono'));
            }
            reports.push({ theme, width, scale, variant, section, ...report, actualFonts });
            await page.screenshot({ path: path.join(artifacts, `${theme}-${variant}-${section}-${width}.png`), fullPage: true });
          }
          await page.evaluate(() => closeDetail());
        }
        const home = await inspectHome(page, cdp);
        reports.push({ theme, width, scale, variant: 'home', section: 'statistics', ...home });
        await page.screenshot({ path: path.join(artifacts, `${theme}-home-statistics-${width}.png`), fullPage: true });
      }
    }
    assert.deepEqual(errors, []);
    fs.writeFileSync(path.join(artifacts, 'report.json'), JSON.stringify(reports, null, 2));
    console.log(JSON.stringify({ result: 'PASS', checked_pages: reports.length, actual_font_verified: true, artifacts }));
  } finally { if (browser) await browser.close(); await new Promise(resolve => server.close(resolve)); }
})().catch(error => { console.error(error); process.exitCode = 1; });
