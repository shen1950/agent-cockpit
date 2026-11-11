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
    await page.locator('#btn-add-app').click({force:true});
    assert.equal(await page.locator('#amodal').isVisible(), true, '添加程序弹窗应打开');
    await page.locator('#am-pick').click();
    await page.waitForFunction(() => document.querySelector('#am-exe').value.length > 0);
    assert.equal(await page.locator('#am-exe').inputValue(), 'C:\\Apps\\Demo\\Demo.exe');
    assert.equal(await page.locator('#am-name').inputValue(), 'Demo');
    console.log('add-app picker: 选择按钮回填路径并自动带出显示名 — pass');
  } finally { await browser.close(); server.close(); }
})().catch(e=>{console.error(e);server.close();process.exitCode=1;});
