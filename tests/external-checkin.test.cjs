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
    const type = {'.html':'text/html','.js':'text/javascript','.css':'text/css','.svg':'image/svg+xml'}[path.extname(p)];
    res.setHeader('Content-Type', type || 'application/octet-stream'); res.end(data);
  });
});
(async () => {
  await new Promise(r => server.listen(0, '127.0.0.1', r));
  const browser = await chromium.launch({channel:'chrome',headless:true});
  try {
    const page = await browser.newPage({viewport:{width:1440,height:1000}});
    await page.goto(`http://127.0.0.1:${server.address().port}/`);
    await page.waitForFunction(() => typeof S !== 'undefined' && S.data);
    await page.evaluate(() => {
      clearInterval(pollTimer);
      window.__calls = [];
      // app.js 顶上是 `let invoke`（不是 window 属性），普通赋值才覆盖得到
      invoke = (cmd, args) => {
        const act = args && args.payload && args.payload.action;
        window.__calls.push({ cmd, action: act, payload: args && args.payload });
        if (act === 'checkin_external')
          return Promise.resolve({ ok: true, message: '已补记 2026-11-04 的外部签到' });
        // checkin_view 等一路挂住：render() 会用 state 报文同步覆盖 S.checkin，
        // 挂住才不会在断言中途把 mock 数据画回来（workbuddy-accounts 同款做法）
        return new Promise(() => {});
      };
      // askConfirm 是 const：确认动作直接点弹窗的「确定」按钮
      window.toast = () => {};
      S.view = 'checkin'; render();
      const common = {product:'workbuddy-intl',app:'workbuddy-intl',today:'2026-11-04',
        supported:true,auto:true,activity_on:true,reward:30,
        signed_today:true,claimable:false,words:{done:'今天已活跃',done_act:'今天已用过'}};
      const qoder = {product:'qoder-cn',app:'qoder-cn',today:'2026-11-04',
        supported:true,auto:true,activity_on:true,reward:15,
        signed_today:false,claimable:true,words:{go:'今天可领',act:'立刻签到'}};
      const logRows = [];
      for (let i = 0; i < 40; i++)
        logRows.push({at:'2026-11-04',store:'qoder-cn',account:'',claimed:true,credit:15,message:'CLAIMED'});
      S.checkin = {ok:true,fetched_at:Math.floor(Date.now()/1000),items:[
        {...common,scope:'product',name:'WorkBuddy 国际版'},
        {...common,scope:'account',parent:'workbuddy-intl',name:'Gerald Fabinhu',account:'Gerald Fabinhu'},
        {...qoder,scope:'product',name:'Qoder CN'}],
        state:{pending:{},log:logRows}};
      renderCheckin();
    });
    // ① 有账号子行的产品，补记只在账号行上；无子行的产品行也有
    const actCells = page.locator('#checkin-body .ci-act');
    assert.equal(await actCells.count(), 3);
    assert.equal(await actCells.nth(0).locator('.ci-ext').count(), 0, '产品行有子行时不给补记');
    assert.equal(await actCells.nth(1).locator('.ci-ext').count(), 1, '账号行给补记');
    assert.equal(await actCells.nth(2).locator('.ci-ext').count(), 1, '无子行的产品行给补记');
    // ② 点补记 → checkin_external 带上 store + account
    await actCells.nth(1).locator('.ci-ext').click();
    await page.locator('#ask-ok').click();
    await page.waitForTimeout(150);
    const sent = await page.evaluate(() =>
      window.__calls.filter(c => c.action === 'checkin_external').map(c => c.payload));
    assert.equal(sent.length, 1);
    assert.equal(sent[0].store, 'workbuddy-intl');
    assert.equal(sent[0].account, 'Gerald Fabinhu');
    // ③ 同一份数据重复 renderCheckin 不重建：滚动位置、DOM 身份都原样
    await page.evaluate(() => {
      const list = document.querySelector('.cilogrows');
      list.scrollTop = 500;
      list.__marker = 'kept';
      renderCheckin();
    });
    const s1 = await page.evaluate(() => {
      const l = document.querySelector('.cilogrows');
      return { marker: l.__marker || '', top: l.scrollTop };
    });
    assert.equal(s1.marker, 'kept', '数据没变就不该整页重建');
    assert.ok(Math.abs(s1.top - 500) <= 1, '未重建时滚动位置不动');
    // ④ 数据变了要重建，且滚动位置带到新列表上
    await page.evaluate(() => {
      S.checkin.state.log.unshift({at:'2026-11-04',store:'qoder-cn',account:'',
        source:'external',message:'官方客户端已签到（驾驶舱读状态时检测到，非驾驶舱领取）'});
      renderCheckin();
    });
    const s2 = await page.evaluate(() => {
      const l = document.querySelector('.cilogrows');
      return { marker: l.__marker || '', top: l.scrollTop };
    });
    assert.notEqual(s2.marker, 'kept', '数据变了要重建');
    assert.ok(s2.top > 0, '重建后日志滚动位置保留');
    // ⑤ 外部签到的行有独立的颜色档和说明文案
    const extRow = page.locator('#checkin-body .cilogrows .cl-r.ext').first();
    assert.match(await extRow.textContent(), /非驾驶舱领取/);
    console.log('external checkin: button gating, payload, scroll kept across re-render, ext tone — all pass');
  } finally { await browser.close(); server.close(); }
})().catch(e=>{console.error(e);server.close();process.exitCode=1;});
