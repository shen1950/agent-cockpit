/* Real Chromium geometry checks against the shipped HTML/CSS/JS with a fake
   local IPC bridge. No real account, credential or client process is accessed. */
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const { chromium } = require('D:/npm-global/node_modules/@playwright/mcp/node_modules/playwright');
const root = path.resolve(__dirname, '../ui');
const artifacts = path.resolve(__dirname, '../.agent-sync/layout-evidence');
const mime = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.svg': 'image/svg+xml', '.woff2': 'font/woff2', '.png': 'image/png' };
const server = http.createServer((req, res) => {
  const requested = decodeURIComponent(new URL(req.url, 'http://localhost').pathname);
  const file = path.resolve(root, '.' + (requested === '/' ? '/index.html' : requested));
  if (file !== root && !file.startsWith(root + path.sep)) { res.writeHead(403).end(); return; }
  fs.readFile(file, (error, bytes) => {
    if (error) { res.writeHead(404).end(); return; }
    res.setHeader('Content-Type', mime[path.extname(file)] || 'application/octet-stream'); res.end(bytes);
  });
});
async function seed(page, zoom) {
  await page.evaluate((scale) => {
    clearInterval(pollTimer); localStorage.setItem('cockpit-toured', '1');
    const source = structuredClone(window.layoutSource ||= S.data.instances[0]);
    const timestamp = Math.floor(Date.now() / 1000);
    const make = (name, mode) => {
      const item = structuredClone(source);
      Object.assign(item, { app: 'codex-desktop', app_name: 'ChatGPT', name, kind: 'pwa', running: true,
        procs: 8, resident: false, swapped: false, in_place: false, created: '2026-10-27',
        data_dir: 'C:\\Users\\Example\\.agent-cockpit\\instances\\codex-desktop\\' + 'very-long-account-directory-'.repeat(5),
        chatgpt_auth: { has_auth: true, mode, native: false, account: mode === 'chatgpt' ? 'very-long-account-identity-without-spaces-'.repeat(3) + '@example.test' : null,
          base_url: 'https://api.example.test/v1', home: 'C:\\Users\\Example\\.agent-cockpit\\chatgpt-auth\\' + 'very-long-account-directory-'.repeat(5),
          history_mode: 'copy', history_snapshot: { threads: 131, projects: 12, visibility_version: 2 },
          project_repair: { pending: true, state: 'pending', repairable_count: 8 } },
        chatgpt_quota: mode === 'apikey'
          ? { state: 'live', mode, fetched_at: timestamp, windows: [], balance: { remaining: 0, total: 100, used: 100, currency: 'CNY' } }
          : { state: 'live', mode, fetched_at: timestamp, balance: null, windows: [
            { id: '5h', label: '5 小时', remaining_percent: 72.5, used_percent: 27.5, reset_at: timestamp + 1800 },
            { id: 'weekly', label: '每周', remaining_percent: 18.2, used_percent: 81.8, reset_at: timestamp + 86400 }
          ] }
      });
      item.quota = { ...item.quota, stale: true, live_error: 'old generic cached text' };
      return item;
    };
    S.data.apps = [{ id: 'codex-desktop', name: 'ChatGPT', short: 'CG', kind: 'pwa', installed: true,
      isolatable: true, swap_ok: false, swapped: '', icon: '' }];
    S.data.instances = [make('long-account-name-without-spaces-'.repeat(3) + '@example.test', 'apikey'), make('Plus 工作账号', 'chatgpt')];
    S.data.settings.appearance = { ...S.data.settings.appearance, theme: 'light', font_scale: scale };
    S.data.stats = { ...S.data.stats, instances: 2, agents: 1, running: 1, running_instances: 2 };
    const fixture = S.data;
    invoke = async (command, args = {}) => {
      if (command === 'get_state') return fixture;
      if (command === 'get_meta') return fixture.instances.find(i => i.name === args.name) || {};
      const input = args.payload || {};
      if (input.action === 'chatgpt_quota_refresh') return { ok: true, quota: fixture.instances.find(i => i.name === input.name)?.chatgpt_quota };
      if (input.action === 'chatgpt_status') return { ok: true, status: fixture.instances.find(i => i.name === input.name)?.chatgpt_auth };
      return { ok: true, msg: '预览操作' };
    };
    S.view = 'agents'; S.cur = 'codex-desktop'; S.busy = false; render();
  }, zoom);
}
async function verifyCards(page) {
  return page.evaluate(() => {
    const cards = [...document.querySelectorAll('#cards > .card')];
    const problems = [];
    let pairs = 0;
    const rect = node => node.getBoundingClientRect();
    for (const card of cards) {
      const children = [...card.children];
      if (children.length !== 3 || !children[0].classList.contains('top') || !children[2].classList.contains('acts')) {
        problems.push(`unexpected card structure: ${children.map(n => n.className).join(', ')}`);
        continue;
      }
      for (const node of [card, ...card.querySelectorAll('.top, .nm, .ident, .quota, .cpt-quota, .acts')]) {
        if (node.scrollWidth > node.clientWidth + 2) problems.push(`horizontal overflow: ${node.className} ${node.scrollWidth}/${node.clientWidth}`);
      }
    }
    // Cards in the same visual row must share boundaries and content rows.
    // Narrow layouts may legitimately use a single column.
    for (let i = 0; i < cards.length; i++) for (let j = i + 1; j < cards.length; j++) {
      const a = cards[i], b = cards[j], ar = rect(a), br = rect(b);
      if (Math.abs(ar.top - br.top) > 2) continue;
      pairs++;
      for (const [label, x, y] of [
        ['card bottom', ar.bottom, br.bottom],
        ['quota top', rect(a.children[1]).top, rect(b.children[1]).top],
        ['quota bottom', rect(a.children[1]).bottom, rect(b.children[1]).bottom],
        ['actions top', rect(a.children[2]).top, rect(b.children[2]).top],
      ]) if (Math.abs(x - y) > 2) problems.push(`${label} differs: ${x} / ${y}`);
    }
    return { count: cards.length, pairs, problems };
  });
}
async function verifyPage(page, section) {
  return page.evaluate((id) => {
    const body = document.getElementById('im-body'), nav = document.getElementById('im-nav');
    const b = body.getBoundingClientRect(), n = nav.getBoundingClientRect();
    const problems = [];
    if (body.scrollWidth > body.clientWidth + 3) problems.push(`body overflow ${body.scrollWidth}/${body.clientWidth}`);
    if (n.right > b.left + 3) problems.push('menu overlaps content');
    for (const node of body.querySelectorAll('.cpt-info-label,.cpt-location h3')) {
      const r = node.getBoundingClientRect();
      if (r.width < 75 || r.height > 70) problems.push(`squeezed label: ${node.textContent}`);
    }
    for (const node of body.querySelectorAll('.cpt-path')) {
      const r = node.getBoundingClientRect();
      if (r.right > b.right + 2 || node.scrollWidth > node.clientWidth + 3) problems.push('path overflows');
    }
    return { section: id, problems, text: body.textContent.trim().length, width: Math.round(b.width) };
  }, section);
}
async function seedZcode(page, zoom) {
  await page.evaluate((scale) => {
    const source = structuredClone(S.data.instances[0]);
    const make = (name, provider, current) => ({ ...source,
      app: 'zcode', app_name: 'ZCode', kind: 'electron', name, running: current, resident: false,
      logged_in: current, released: false, swapped: false, in_place: true,
      data_dir: 'C:\\Users\\Example\\.zcode',
      chatgpt_auth: null, chatgpt_quota: null,
      zcode_auth: { provider, account: name, has_auth: true, current, auth_status: 'ready',
        user_id: `zcode:${provider}:demo`, native_data_dir: 'C:\\Users\\Example\\.zcode' },
      quota: { ...source.quota, stale: false, live_error: '', web_error: '', groups: [], portions: [],
        left: 2735348, total: 8000000, local_account: name, source: 'server' },
    });
    S.data.apps = [{ id: 'zcode', name: 'ZCode', short: 'ZC', kind: 'electron', installed: true,
      isolatable: false, shared_profile: true, auth_switch: true, local_quota: true, swap_ok: false,
      machine: { known: true, value: { ok: true, account: 'Z.ai 工作账号', user_id: 'zcode:zai:demo' } } }];
    S.data.instances = [make('BigModel 工作账号', 'bigmodel', false), make('Z.ai 工作账号', 'zai', true)];
    S.data.settings.appearance = { ...S.data.settings.appearance, theme: 'light', font_scale: scale };
    const fixture = S.data;
    invoke = async (command, args = {}) => command === 'get_state' ? fixture
      : command === 'get_meta' ? fixture.instances.find(i => i.name === args.name) || {} : { ok: true };
    S.view = 'agents'; S.cur = 'zcode'; S.busy = false; render();
  }, zoom);
}
async function verifyClaudeDesktop(page, zoom) {
  const result = await page.evaluate((scale) => {
    const calls = [];
    S.data.apps = [{ id: 'claude-desktop', name: 'Claude 桌面版', kind: 'plain', installed: true,
      isolatable: false, native_data_dir: 'C:\\Users\\Example\\AppData\\Local\\Claude-3p' }];
    S.data.instances = [];
    S.data.settings.appearance = { ...S.data.settings.appearance, font_scale: scale };
    invoke = async (command, args = {}) => {
      if (command === 'get_state') return S.data;
      calls.push({ command, ...args });
      return { ok: true, msg: '已打开' };
    };
    S.view = 'agents'; S.cur = 'claude-desktop'; render();
    const panel = document.querySelector('[data-product="claude-desktop"]');
    const result = { text: panel?.textContent, cap: document.getElementById('cur-cap').textContent,
      navCap: document.querySelector('.nav-btn[data-app="claude-desktop"] .cap')?.textContent,
      path: panel?.querySelector('.cpt-path')?.textContent, calls, overflow: panel ? panel.scrollWidth > panel.clientWidth + 3 : true };
    document.getElementById('btn-new').click();
    panel?.querySelector('button:not(.qhelp)')?.click();
    result.wizardHidden = document.getElementById('wizard').hidden;
    renderHome(S.data);
    result.home = [...document.querySelectorAll('.hagent')].find(node => node.querySelector('.hagent-n')?.textContent === 'Claude 桌面版')?.textContent;
    return result;
  }, zoom);
  assert.ok(result.text?.includes('资料位置'), JSON.stringify(result));
  assert.equal(result.cap, '桌面客户端');
  assert.equal(result.navCap, '客户端');
  assert.ok(result.home?.includes('桌面客户端') && !/换号|还没有账号/.test(result.home), JSON.stringify(result));
  assert.equal(result.path, 'C:\\Users\\Example\\AppData\\Local\\Claude-3p');
  assert.equal(result.overflow, false);
  assert.equal(result.wizardHidden, true, 'desktop launch must not create an account wizard');
  assert.deepEqual(result.calls.map(c => c.payload?.action), ['launch_app', 'claude_desktop_open_data']);
  assert.ok(result.calls.every(c => c.payload.app === 'claude-desktop' && !c.payload.name && !c.payload.path));
  return { product: 'claude-desktop', section: 'client', problems: [], width: await page.evaluate(() => innerWidth) };
}
(async () => {
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  fs.mkdirSync(artifacts, { recursive: true });
  let browser;
  try {
    browser = await chromium.launch({ channel: 'chrome', headless: true });
    const page = await browser.newPage();
    const errors = []; page.on('pageerror', e => errors.push(e.message));
    await page.addInitScript(() => localStorage.setItem('cockpit-toured', '1'));
    await page.goto(`http://127.0.0.1:${server.address().port}/`, { waitUntil: 'networkidle' });
    await page.waitForFunction(() => typeof S !== 'undefined' && S.data && !S.busy);
    const reports = [];
    for (const [width, height, zoom] of [[1320, 920, 100], [960, 850, 125]]) {
      await page.setViewportSize({ width, height }); await seed(page, zoom);
      await page.screenshot({ path: path.join(artifacts, `cards-${width}.png`), fullPage: true });
      const cardsReport = await verifyCards(page);
      assert.equal(cardsReport.count, 2);
      if (width === 1320) assert.ok(cardsReport.pairs > 0, 'wide layout should show a pair of cards');
      assert.deepEqual(cardsReport.problems, [], JSON.stringify({ product: 'chatgpt', width, zoom, ...cardsReport }));
      const plus = 'Plus 工作账号';
      for (const [section, label] of [['info','概览'],['quota','额度与用量'],['run','运行管理'],['history','对话与项目'],['data','文件位置'],['auth','登录管理'],['danger','移除账号']]) {
        await page.evaluate(async ({ name, sec }) => openDetail('codex-desktop', name, sec), { name: plus, sec: section });
        const report = await verifyPage(page, section); reports.push({ width, zoom, ...report });
        assert.equal(report.problems.length, 0, JSON.stringify({ width, zoom, ...report })); assert.ok(report.text > 10);
        assert.deepEqual(await page.locator('#im-nav button').allTextContents(), ['概览','额度与用量','运行管理','对话与项目','文件位置','登录管理','移除账号']);
        if (['data', 'quota'].includes(section)) await page.screenshot({ path: path.join(artifacts, `${section}-${width}.png`), fullPage: true });
      }
      await page.evaluate(async () => { await closeDetail(); await openDetail('codex-desktop', S.data.instances[0].name, 'auth'); });
      assert.equal(await page.locator('#im-body input[type=password]').count(), 0, 'saved API credentials should not open an empty login form');
      await page.getByRole('button', { name: '更换 API Key', exact: true }).click();
      assert.equal(await page.locator('#im-body input[type=password]').count(), 1);
      await page.evaluate(() => closeDetail());
      await seedZcode(page, zoom);
      await page.screenshot({ path: path.join(artifacts, `zcode-cards-${width}.png`), fullPage: true });
      const zcodeCardsReport = await verifyCards(page);
      assert.equal(zcodeCardsReport.count, 2);
      assert.deepEqual(zcodeCardsReport.problems, [], JSON.stringify({ product: 'zcode', width, zoom, ...zcodeCardsReport }));
      for (const section of ['info', 'quota', 'run', 'auth', 'data', 'danger']) {
        await page.evaluate(async (sec) => openDetail('zcode', 'Z.ai 工作账号', sec), section);
        const report = await verifyPage(page, section); reports.push({ product: 'zcode', width, zoom, ...report });
        assert.equal(report.problems.length, 0, JSON.stringify(report));
        assert.deepEqual(await page.locator('#im-nav button').allTextContents(), ['概览','额度与用量','运行管理','登录管理','文件位置','移除账号']);
        if (['auth', 'run'].includes(section)) await page.screenshot({ path: path.join(artifacts, `zcode-${section}-${width}.png`), fullPage: true });
      }
      await page.evaluate(() => closeDetail());
      reports.push({ zoom, ...await verifyClaudeDesktop(page, zoom) });
      await page.screenshot({ path: path.join(artifacts, `claude-desktop-${width}.png`), fullPage: true });
    }
    await page.setViewportSize({ width: 420, height: 850 });
    await seed(page, 100);
    const narrowCards = await verifyCards(page);
    assert.equal(narrowCards.count, 2);
    assert.deepEqual(narrowCards.problems, [], JSON.stringify({ product: 'chatgpt', width: 420, ...narrowCards }));
    assert.deepEqual(errors, [], 'no frontend runtime errors');
    fs.writeFileSync(path.join(artifacts, 'geometry.json'), JSON.stringify(reports, null, 2));
    console.log(JSON.stringify({ checked_pages: reports.length, screenshots: artifacts, result: 'PASS' }));
  } finally { if (browser) await browser.close(); await new Promise(resolve => server.close(resolve)); }
})().catch(error => { console.error(error); process.exitCode = 1; });
