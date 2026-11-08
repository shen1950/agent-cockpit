/* Browser contract: the wizard reuses the Agent gateway panel; routes are server events, never generated demos. */
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const {chromium} = require('playwright');
const root = path.resolve(__dirname, '../ui');
const evidence = path.resolve(__dirname, '../.agent-sync/gateway-route-evidence');
const server = http.createServer((req, res) => {
  const pathname = decodeURIComponent(new URL(req.url, 'http://localhost').pathname);
  const file = path.resolve(root, '.' + (pathname === '/' ? '/index.html' : pathname));
  if (!file.startsWith(root + path.sep)) return res.writeHead(403).end();
  fs.readFile(file, (error, bytes) => {
    if (error) return res.writeHead(404).end();
    res.setHeader('Content-Type', {'.html':'text/html','.js':'text/javascript','.css':'text/css','.svg':'image/svg+xml','.png':'image/png'}[path.extname(file)] || 'application/octet-stream');
    res.end(bytes);
  });
});
(async () => {
  await new Promise(r => server.listen(0, '127.0.0.1', r));
  let browser;
  try {
    browser = await chromium.launch({channel:'chrome',headless:true});
    const page = await browser.newPage({viewport:{width:1360,height:1000}});
    const errors=[];
    page.on('pageerror', e => errors.push(e.message));
    await page.addInitScript(() => localStorage.setItem('cockpit-toured','1'));
    await page.goto(`http://127.0.0.1:${server.address().port}`, {waitUntil:'networkidle'});
    await page.waitForFunction(() => typeof S !== 'undefined' && S.data && !S.busy);
    await page.evaluate(() => {
      clearInterval(pollTimer);
      window.gatewayRouteCalls=[];
      window.gatewayRouteResult={requests:[],total:0,rerouted:0,errors:0};
      const real=invoke;
      invoke=async (cmd,args) => {
        if (cmd!=='client_gateway') return real(cmd,args);
        window.gatewayRouteCalls.push(args);
        if (args.op==='routes') return JSON.parse(JSON.stringify(window.gatewayRouteResult));
        if (args.op==='status') return {supported:true,connected:false,managed:false,single:false};
        if (args.op==='models') return {models:['trae/vision-fixture','workbuddy/text-fixture'],details:[
          {id:'trae/vision-fixture',owned_by:'trae',native_vision:true,native_image_transport:true,vision:true,context_window:1000000,max_output_tokens:64000},
          {id:'workbuddy/text-fixture',owned_by:'workbuddy',native_vision:false,native_image_transport:true,vision_model:'trae/vision-fixture'}],policies:{}};
        if (args.op==='provider_info') return {rule:{},price:null,official:null,effective:null,groups:[],summary:{requests:0,priced:0,usd:0,missing_usage:0,missing_price:0}};
        return {ok:true};
      };
      S.data.apps=[{id:'codex',name:'Codex',installed:true},{id:'workbuddy',name:'WorkBuddy',installed:true},{id:'traework-cn',name:'TRAE CN',installed:false}];
      S.data.proxy.running=true; S.data.proxy.port=8788;
      S.data.proxy.dist=[{id:'fixture-key',name:'本机测试密钥',enabled:true}];
      S.data.settings.appearance.theme='light';
      S.view='proxy'; render();
    });
    const guide=page.locator('#proxy-guide');
    await guide.getByRole('button',{name:'第 4 步：接入本地网关',exact:true}).click();
    await page.waitForSelector('#proxy-gateway-client h2');
    assert.equal(await guide.getByLabel('接入 Agent',{exact:true}).inputValue(),'codex');
    assert.equal(await guide.locator('h2').textContent(),'接入本地网关');
    assert.equal(await guide.getByText('这里提供通用配置，不会自动写入客户端。',{exact:false}).count(),0);
    await guide.getByLabel('接入 Agent',{exact:true}).selectOption('workbuddy');
    await page.waitForFunction(() => window.gatewayRouteCalls.some(c=>c.app==='workbuddy' && c.op==='status'));
    const panel=page.locator('#proxy-gateway-client');
    // 接入面板已不再有「高级模型设置」折叠块（上下文窗口等策略在供应商详情里），
    // 草稿保留改用「接入模型」选择器：选完失焦，轮询渲染不得重建节点或重置选择。
    await panel.getByLabel('接入模型',{exact:true}).selectOption('trae/vision-fixture');
    // Moving focus away permits the normal page poll path, which must preserve the selection and node.
    await panel.getByRole('heading',{name:'接入本地网关'}).click();
    const stable=await page.evaluate(() => {window.savedGatewayNode=document.querySelector('#proxy-gateway-client');render();return window.savedGatewayNode===document.querySelector('#proxy-gateway-client');});
    assert.equal(stable,true);
    assert.equal(await panel.getByLabel('接入模型',{exact:true}).inputValue(),'trae/vision-fixture');
    await panel.getByRole('button',{name:'一键接入',exact:true}).click();
    await page.waitForFunction(() => window.gatewayRouteCalls.some(c=>c.app==='workbuddy' && c.op==='connect' && c.keyId==='fixture-key'));
    await page.locator('#proxy-routes').getByText('还没有请求',{exact:true}).waitFor();
    assert.equal(await page.locator('.gateway-route-attempt').count(),0);
    await page.evaluate(async () => {
      const now=Date.now();
      window.gatewayRouteResult={total:2,rerouted:1,errors:1,requests:[
        {id:'r2',started_at:now,agent:'WorkBuddy',model:'trae/vision-fixture',status:'completed',attempts:[
          {model:'trae/vision-fixture',provider:'trae',account:'账号 a1b2…',state:'failed',reason:'上游限流，尝试备用模型',status:429},
          {model:'workbuddy/text-fixture',provider:'workbuddy',account:'账号 c3d4…',state:'completed',reason:'备用模型',status:200}]},
        {id:'r1',started_at:now-10000,agent:'Codex',model:'trae/vision-fixture',status:'failed',error:'HTTP 503',attempts:[]}]};
      await GatewayRoutes.refresh();
    });
    assert.equal(await page.locator('.gateway-route-attempt').count(),2);
    assert.match(await page.locator('#proxy-routes').textContent(),/上游限流/);
    const numbers=await page.locator('.gateway-route-stats b').allTextContents();
    assert.deepEqual(numbers,['2','1','1']);
    // 定高 flex 列里 overflow:hidden 的面板会被压扁成标题条；路由区不许这样
    const box=await page.locator('#proxy-routes').evaluate(e=>({client:e.clientHeight,scroll:e.scrollHeight}));
    assert.ok(box.scroll<=box.client+2,`路由区不能被 flex 压扁：${JSON.stringify(box)}`);
    fs.mkdirSync(evidence,{recursive:true});
    await page.locator('#proxy-routes').screenshot({path:path.join(evidence,'gateway-route-light.png')});
    await page.locator('[data-request-id="r1"]').click();
    assert.equal(await page.locator('.gateway-route-toolbar input').isChecked(),false);
    await page.evaluate(async () => {window.gatewayRouteResult.total=3;await GatewayRoutes.refresh();});
    assert.match(await page.locator('#proxy-routes').textContent(),/HTTP 503/);
    assert.equal(await page.locator('.gateway-route-attempt').count(),0);
    await page.locator('.gateway-route-toolbar input').check();
    assert.equal(await page.locator('.gateway-route-attempt').count(),2);
    await page.evaluate(() => {S.data.settings.appearance.theme='dark';render();});
    await page.locator('#proxy-routes').screenshot({path:path.join(evidence,'gateway-route-dark.png')});
    await page.setViewportSize({width:760,height:1000});
    const fits=await page.locator('#proxy-routes').evaluate(e=>e.scrollWidth<=e.clientWidth+1);
    assert.equal(fits,true,'路由图不得撑破窄窗布局');
    await page.locator('#proxy-routes').screenshot({path:path.join(evidence,'gateway-route-narrow.png')});
    const before=await page.evaluate(()=>{S.view='home';render();return window.gatewayRouteCalls.filter(c=>c.op==='routes').length;});
    await page.waitForTimeout(2300);
    assert.equal(await page.evaluate(()=>window.gatewayRouteCalls.filter(c=>c.op==='routes').length),before,'离开页面后应停止路由轮询');
    assert.deepEqual(errors,[]);
    console.log(JSON.stringify({result:'PASS',evidence,checks:['shared client setup','selection persistence','real event rendering','request selection','light/dark/narrow','poll lifecycle']}));
  } finally {
    if(browser)await browser.close();
    await new Promise(r=>server.close(r));
  }
})().catch(error=>{console.error(error);process.exitCode=1;});
