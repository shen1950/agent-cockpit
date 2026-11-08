/* 无头渲染一遍「积分明细」，确认编辑入口与「添加积分包」真的长在界面上。
   跑法（两条要在同一个 shell 里，服务是前台进程，别指望它跨命令活着）：
     python -m http.server 8899 --directory ui > /tmp/h.log 2>&1 & sleep 2; \
       node tests/expiry-ui-live.cjs
   （mock.js 在没有 window.__TAURI__ 时会被 app.js 自动加载，所以这条不碰真后端；
     本机只装了 playwright 的 node 包、没下 chromium，所以直接用系统 Chrome/Edge） */
const { chromium } = require('playwright');

(async () => {
  // 本机没装 playwright 自带的那份 chromium（只装了 node 包）：直接用系统 Chrome，
  // 免得为一个一次性的渲染检查去下 100MB。没有 Chrome 就退回 Edge。
  const exe = ['C:/Program Files/Google/Chrome/Application/chrome.exe',
    'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe']
    .find((p) => require('node:fs').existsSync(p));
  const browser = await chromium.launch(exe ? { executablePath: exe } : {});
  const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
  const errors = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  // 新手引导会在 600ms 后自己起来，它的遮罩盖在所有弹层之上，
  // 会把命中测试全挡在 tourmask 上——先标记"已看过引导"，再进页面。
  await page.addInitScript(() => localStorage.setItem('cockpit-toured', '1'));
  await page.goto('http://127.0.0.1:8899/index.html', { waitUntil: 'load' });
  await page.waitForTimeout(1200);

  const out = await page.evaluate(() => {
    const inst = {
      app: 'qoder', app_name: 'Qoder', name: 'SOTA', kind: 'electron',
      quota: { fetched_at: 1, groups: [
        { name: '套餐基础积分', orig: '套餐基础积分', gid: 'g:套餐基础积分',
          unit: '积分', left: 0, total: 0, used: 0, live_count: 0, expired_count: 1,
          expires: '', days_left: null, items: [] },
        { name: '其他', orig: '其他', gid: 'g:其他', renamed: true,
          unit: '积分', left: 2000, total: 2000, used: 0, live_count: 1, expired_count: 0,
          expires: '2026-11-08 22:22', days_left: 3, expires_manual: true,
          items: [{ label: 'SOTA', left: 2000, total: 2000, used: 0, unit: '积分',
                    expires_on: '2026-11-08 22:22', days_left: 3, expired: false }] },
        { name: '手工包', orig: '手工包', gid: 'c:c1', custom: true, custom_id: 'c1',
          unit: '积分', left: 500, total: 500, used: 0, live_count: 0, expired_count: 0,
          expires: '', days_left: null, expires_manual: true, items: [] },
      ] },
    };
    S.data = S.data || {};
    S.data.instances = (S.data.instances || []).filter((x) => x.app !== 'qoder').concat([inst]);
    const host = document.createElement('div');
    host.id = 'pack-probe';
    document.body.appendChild(host);
    host.appendChild(quotaPanel(inst));
    const heads = [...host.querySelectorAll('.qghead')].map((h) => h.innerText.replace(/\n/g, ' '));
    const pencils = [...host.querySelectorAll('.qghead .iconbtn')].length;
    const addBtn = [...host.querySelectorAll('.qgadd button')].map((b) => b.textContent.trim());
    const tags = [...host.querySelectorAll('em')].map((e) => e.textContent.trim()).filter(Boolean);
    return { text: host.innerText, heads, pencils, addBtn, tags };
  });

  // 真的点一下外层档的铅笔，确认弹窗开了、字段是这几格
  await page.evaluate(() => {
    const b = document.querySelectorAll('#pack-probe .qghead .iconbtn')[1];
    b.click();
  });
  const modal = await page.evaluate(() => ({
    open: !document.getElementById('qpmodal').hidden,
    title: document.getElementById('qp-title').textContent,
    name: document.getElementById('qp-name').value,
    dateHidden: document.getElementById('qp-date').hidden,
    totalHidden: document.getElementById('qp-total').hidden,
    delHidden: document.getElementById('qp-del').hidden,
    hint: document.getElementById('qp-hint').textContent,
  }));

  // 再开「查看明细」，确认内层每一笔都有自己的铅笔，且点开能改截止日期
  const inner = await page.evaluate(() => {
    document.getElementById('qpmodal').hidden = true;
    openQuotaDetail(S.data.instances.find((x) => x.app === 'qoder'),
      quotaGroups(S.data.instances.find((x) => x.app === 'qoder'))[1]);
    const rows = [...document.querySelectorAll('#qm-list .qitem')];
    const pencils = rows.map((r) => r.querySelectorAll('.iconbtn').length);
    rows[0].querySelector('.iconbtn').click();
    const ed = document.getElementById('qpmodal');
    const sheet = document.getElementById('qmodal');
    // 真正的问题：两个弹层都开着时，点编辑弹窗里的输入框，命中的必须是它自己，
    // 不能是盖在上面的明细层。用 elementFromPoint 验——这是浏览器真实的命中测试。
    const probe = document.getElementById('qp-name');
    const b = probe.getBoundingClientRect();
    const hit = document.elementFromPoint(b.left + b.width / 2, b.top + b.height / 2);
    return {
      rowCount: rows.length,
      pencilsPerRow: pencils,
      title: document.getElementById('qp-title').textContent,
      dateHidden: document.getElementById('qp-date').hidden,
      dateRowHidden: document.getElementById('qp-date-row').hidden,
      hint: document.getElementById('qp-hint').textContent,
      modalOpen: !ed.hidden,
      sheetStillOpen: !sheet.hidden,
      editorZ: getComputedStyle(ed).zIndex,
      sheetZ: getComputedStyle(sheet).zIndex,
      // 命中测试：落在编辑弹窗的输入框上，而不是被明细层抢走
      hitIsEditor: hit === probe,
      hitId: hit ? (hit.id || hit.className) : '',
      // 原生日期框不该在弹窗里露出任何可见面积
      nativeVisible: (() => {
        const n = document.getElementById('qp-native');
        const r = n.getBoundingClientRect();
        return r.width > 0 && r.height > 0 && getComputedStyle(n).opacity !== '0';
      })(),
    };
  });

  console.log(JSON.stringify({ ...out, modal, inner, errors }, null, 1));
  await browser.close();
})();
