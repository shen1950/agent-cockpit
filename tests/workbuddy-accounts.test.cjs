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
      S.view = 'checkin'; render();
      const common = {product:'workbuddy-intl',app:'workbuddy-intl',today:'2026-11-04',
        supported:true,auto:true,activity_on:true,reward:30,note:'每个账号独立追踪到账',
        signed_today:true,claimable:false,words:{done:'今天已活跃',done_act:'今天已用过'}};
      S.checkin = {ok:true,items:[{...common,scope:'product',name:'WorkBuddy 国际版'},
        {...common,scope:'account',parent:'workbuddy-intl',name:'Gerald Fabinhu',account:'Gerald Fabinhu'},
        {...common,scope:'account',parent:'workbuddy-intl',name:'shen1950',account:'shen1950',signed_today:false,claimable:true}],
        state:{pending:{'workbuddy-intl@Gerald Fabinhu|2026-11-04':{label:'WorkBuddy 国际版 · Gerald Fabinhu',expect:'今天这份积分'}},log:[]}};
      renderCheckin();
    });
    for (const width of [1440,1280,1080]) {
      await page.setViewportSize({width,height:1000});
      const bounds = await page.locator('.ci-nm').evaluateAll(nodes => nodes.map(n=>({name:n.textContent,visible:n.clientWidth,full:n.scrollWidth})));
      for (const b of bounds) assert(b.full <= b.visible+1, `${width}px clips ${b.name}: ${JSON.stringify(b)}`);
    }
    assert.equal(await page.locator('.cipend .cihit').count(), 1);
    assert.match(await page.locator('.ci-badge').first().textContent(), /1\/2/);
    await page.setViewportSize({width:1440,height:1000});
    const out = path.resolve(__dirname,'../.agent-sync/workbuddy-evidence');
    fs.mkdirSync(out,{recursive:true});
    await page.locator('.cipad').screenshot({path:path.join(out,'checkin-layout.png')});
    console.log('WorkBuddy layout: names complete at 1080/1280/1440px; account states separate; one pending entry.');
  } finally { await browser.close(); server.close(); }
})().catch(e=>{console.error(e);server.close();process.exitCode=1;});
