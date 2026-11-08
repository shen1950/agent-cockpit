/* Actual browser fonts and menu alignment. Uses only the local preview bridge. */
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const { chromium } = require('playwright');
const root = path.resolve(__dirname, '../ui');
const artifacts = path.resolve(__dirname, '../.agent-sync/settings-evidence');
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

(async () => {
 await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
 fs.mkdirSync(artifacts, {recursive:true});
 let browser;
 try {
  browser = await chromium.launch({channel:'chrome',headless:true});
  const page = await browser.newPage();
  const errors=[]; page.on('pageerror',e=>errors.push(e.message));
  await page.addInitScript(()=>localStorage.setItem('cockpit-toured','1'));
  await page.goto(`http://127.0.0.1:${server.address().port}/`);
  await page.waitForFunction(()=>typeof S!=='undefined' && S.data && !S.busy);
  await page.evaluate(()=>{clearInterval(pollTimer); S.storage={root:'C:/Example/AgentCockpit',drive:'C',free_gb:100,sys_drive:'C',sys_free_gb:100,inbound:[]}; S.vault={supported:true,count:0}; S.view='settings'; setSection='about';render();});
  assert.equal(await page.locator('#set-nav button[aria-current="true"]').getAttribute('data-sec'),'about');
  assert(await page.locator('.about-hero img').evaluate(img=>img.complete && img.naturalWidth>0));
  assert.equal(await page.locator('.about-repo').getAttribute('href'),'https://github.com/shen1950/agent-cockpit');
  for (const theme of ['light','dark']) {
   for (const [width,height,scale] of [[1440,1000,100],[960,850,130]]) {
    await page.setViewportSize({width,height});
    await page.evaluate(({theme,scale})=>{S.data.settings.appearance.theme=theme;S.data.settings.appearance.font_scale=scale;applyAppearance(S.data.settings.appearance);},{theme,scale});
    for (const section of ['profile','general','appearance','quota','ai','storage','security','advanced','logs','help','about']) {
     await page.locator(`#set-nav button[data-sec="${section}"]`).click();
     await page.waitForTimeout(80);
     assert.equal(await page.locator('#set-nav button[aria-current="true"]').getAttribute('data-sec'),section);
     const overflow = await page.locator('#set-body').evaluate(e=>e.scrollWidth-e.clientWidth);
     if(overflow>2) console.log(await page.locator('#set-body').evaluate(e=>[...e.querySelectorAll('*')].filter(n=>n.getBoundingClientRect().right>e.getBoundingClientRect().right).map(n=>n.outerHTML.slice(0,220))));
     assert(overflow<=2,`${theme} ${width} ${scale} ${section}: overflow ${overflow}`);
    }
    await page.screenshot({path:path.join(artifacts,`about-${theme}-${width}.png`)});
   }
  }
  await page.locator('#set-nav button[data-sec="help"]').click();
  await page.locator('.help-search').fill('换号');
  assert(await page.locator('.help-questions details').count()>0);
  await page.locator('.help-search').fill('no-matching-question');
  assert.equal(await page.locator('.help-questions details').count(),0);
  await page.locator('.help-search').fill('');
  await page.locator('.help-questions summary').first().click();
  const expanded = await page.locator('.help-questions details').first().evaluate(e=>e.open);
  await page.evaluate(()=>render());
  assert.equal(await page.locator('.help-questions details').first().evaluate(e=>e.open),expanded);
  await page.locator('#set-nav button[data-sec="about"]').click();
  await page.evaluate(()=>{window.__TAURI_INTERNALS__={}; window.settingsCalls=[];invoke=async(c,args)=>{settingsCalls.push(args.payload);return {ok:true};};});
  await page.locator('.about-repo').click();
  assert.deepEqual(await page.evaluate(()=>settingsCalls[0]),{action:'open_project_link',path:''});
  assert.deepEqual(errors,[]);
  console.log('PASS: all 11 sections, light/dark, 100/130% scale, navigation, FAQ search/state, native link dispatch; screenshots in .agent-sync/settings-evidence');
 } finally { if(browser) await browser.close();server.close(); }
})().catch(e=>{console.error(e);process.exitCode=1;});
