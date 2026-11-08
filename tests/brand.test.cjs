/* Actual browser fonts and menu alignment. Uses only the local preview bridge. */
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const { chromium } = require('playwright');
const root = path.resolve(__dirname, '../ui');
const artifacts = path.resolve(__dirname, '../.agent-sync/brand-evidence');
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


(async()=>{
 await new Promise(r=>server.listen(0,'127.0.0.1',r)); fs.mkdirSync(artifacts,{recursive:true});
 let browser;
 try {
  browser=await chromium.launch({channel:'chrome',headless:true}); const page=await browser.newPage({viewport:{width:1440,height:1000}});
  const errors=[];page.on('pageerror',e=>errors.push(e.message));
  await page.addInitScript(()=>localStorage.setItem('cockpit-toured','1'));
  await page.goto(`http://127.0.0.1:${server.address().port}/`);await page.waitForFunction(()=>typeof S!=='undefined' && S.data && !S.busy);
  await page.evaluate(()=>{clearInterval(pollTimer);S.view='settings';setSection='appearance';render();});
  for(const theme of ['light','dark']){
   await page.evaluate(theme=>{S.data.settings.appearance.theme=theme;S.data.settings.appearance.icon_tone='auto';applyAppearance(S.data.settings.appearance);renderSettings();},theme);
   assert.equal(await page.locator('.brand .mark').getAttribute('src'),`brand-${theme}.png`);
   await page.waitForTimeout(250);
   await page.screenshot({path:path.join(artifacts,`icons-${theme}.png`)});
  }
  await page.getByLabel('图标外观',{exact:true}).selectOption('light');
  await page.waitForFunction(()=>document.querySelector('.brand .mark').getAttribute('src')==='brand-light.png');
  await page.evaluate(()=>{S.data.settings.appearance.theme='dark';applyAppearance(S.data.settings.appearance);});
  assert.equal(await page.locator('.brand .mark').getAttribute('src'),'brand-light.png');
  await page.getByLabel('上传浅色图标',{exact:true}).setInputFiles(path.join(root,'brand-dark.png'));
  await page.waitForFunction(()=>S.data.settings.appearance.icon_style==='custom');
  assert((await page.locator('.brand .mark').getAttribute('src')).startsWith('data:image/png;base64,'));
  await page.evaluate(()=>settingsNavigate('about'));
  assert.equal(await page.locator('.about-hero img').getAttribute('src'),await page.locator('.brand .mark').getAttribute('src'));
  await page.evaluate(async()=>{window.__TAURI_INTERNALS__={};window.iconCalls=[];const original=invoke;invoke=async(cmd,args)=>{if(cmd==='set_brand_icon'){iconCalls.push(args.rgba.length);return;}return original(cmd,args);};brandSource='';applyBrand(S.data.settings.appearance);await brandNativeQueue;});
  assert.deepEqual(await page.evaluate(()=>iconCalls),[256*256*4]);
  await page.evaluate(()=>settingsNavigate('appearance'));
  await page.getByRole('button',{name:'恢复 Copper 默认图标',exact:true}).click();
  await page.waitForFunction(()=>S.data.settings.appearance.icon_tone==='auto' && S.data.settings.appearance.icon_style==='copper');
  assert.equal(await page.locator('.brand .mark').getAttribute('src'),'brand-dark.png');
  const invalid=await page.evaluate(()=>importBrand('light',new File(['x'],'x.txt',{type:'text/plain'})).then(()=>false,()=>true)); assert(invalid);
  assert.deepEqual(errors,[]); console.log('PASS brand: theme follow, fixed tone, custom upload, About sync, native RGBA dispatch, reset, invalid upload.');
 }finally{if(browser)await browser.close();server.close();}
})().catch(e=>{console.error(e);process.exitCode=1;});
