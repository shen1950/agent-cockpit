const assert=require('node:assert/strict');
const fs=require('node:fs');
const path=require('node:path');
const http=require('node:http');
const {chromium}=require('playwright');
const root=path.resolve(__dirname,'..');

/* The supplier page keeps UI state in localStorage, so the harness needs a real
   http origin; an about:blank document denies localStorage access. */
const PAGE='<!doctype html><html lang="zh-CN" data-theme="light"><body style="margin:24px"><section id="suppliers" class="panel"></section><section id="pricing" class="panel"></section></body></html>';
const server=http.createServer((req,res)=>{res.setHeader('Content-Type','text/html; charset=utf-8');res.end(PAGE);});

/* Capability and scheduling editors now live in the supplier detail page
   (模型反代 → 供应商 → 详情). The Agent page panel only points there. */

(async()=>{
  const browser=await chromium.launch({channel:'chrome',headless:true});
  try{
    const page=await browser.newPage({viewport:{width:1280,height:1000}});
    const errors=[];page.on('pageerror',e=>errors.push(e.message));
    await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
    await page.goto('http://127.0.0.1:'+server.address().port+'/',{waitUntil:'domcontentloaded'});
    for(const file of ['styles.css','client-connect.css','gateway-provider.css','gateway-suppliers.css'])
      await page.addStyleTag({path:path.join(root,'ui',file)});
    await page.evaluate(()=>{
      window.$=id=>document.getElementById(id);
      window.el=(tag,attrs={},...kids)=>{
        const n=document.createElement(tag);
        for(const [k,v] of Object.entries(attrs)){
          if(v===null||v===undefined||v===false)continue;
          if(k==='class')n.className=v;
          else if(k==='aria')Object.entries(v).forEach(([a,b])=>n.setAttribute('aria-'+a,b));
          else n[k]=v;
        }
        for(const kid of kids.flat())if(kid!==null&&kid!==undefined&&kid!==false)n.append(typeof kid==='object'?kid:document.createTextNode(String(kid)));
        return n;
      };
      window.clear=n=>n.replaceChildren();
      window.toast=()=>{};
      window.btn=(label,cls,handler,title)=>{
        const b=el('button',{class:`btn ${cls||''}`.trim(),title},label);
        b.addEventListener('click',handler);
        return b;
      };
      window.models=[
        {id:'trae/eye',owned_by:'trae',native_vision:true,native_image_transport:true,vision:true,context_window:1000000,max_output_tokens:64000},
        {id:'opencode/text',owned_by:'opencode',native_vision:false,native_image_transport:false,vision:true,vision_model:'trae/eye'},
      ];
      window.calls=[];window.policies={};window.rules={};window.prices={};
      window.S={cur:'workbuddy',view:'proxy',data:{proxy:{running:true,port:8788,dist:[{id:'primary',name:'测试密钥',enabled:true}]}}};
      window.__TAURI__={core:{invoke:async(command,args={})=>{
        window.calls.push({command,...structuredClone(args)});
        if(command==='action'){
          if(args.payload?.action==='proxy_providers')
            return {ok:true,msg:JSON.stringify({providers:[{id:'pv-1',src:'instance:trae/账号1',name:'TRAE/账号1',kind:'instance',app:'traework-cn',app_name:'TRAE CN',has_token:true,supported:true,off:false}]})};
          return {ok:true,msg:''};
        }
        if(command==='client_gateway'){
          const op=args.op;
          if(op==='models')return {models:window.models.map(m=>m.id),details:window.models,policies};
          if(op==='save_policy'){policies[args.model]=args.policy||{};return {ok:true};}
          if(op==='provider_info')return {
            rule:rules[args.model.split('/')[0]]||{concurrency:0,accounts:0,rpm:0,queue_limit:0,queue_wait:0,fallbacks:[],order:'listed',multiplier:null,free:false,periods:[]},
            price:prices[args.model]||null,official:null,effective:null,groups:[{id:'backup',name:'备用组'}],
            summary:{requests:3,priced:1,usd:0.000123,missing_usage:1,missing_price:1}};
          if(op==='save_provider'){if(args.scope==='provider')rules[args.model.split('/')[0]]=args.settings;else prices[args.model]=args.settings;return {ok:true};}
          if(op==='refresh_prices')return {count:10};
          return {ok:true};
        }
        return {ok:true};
      }}};
    });
    for(const file of ['gateway-provider.js','gateway-suppliers.js'])
      await page.addScriptTag({path:path.join(root,'ui',file)});
    await page.evaluate(()=>{window.editorCreations=0;const orig=gatewayProvider.create;gatewayProvider.create=(...a)=>{window.editorCreations++;return orig(...a);};});
    await page.evaluate(async()=>{
      window.page=gatewaySuppliers.create($('suppliers'),{id:'workbuddy'});
      window.page.load();
      await new Promise(r=>setTimeout(r,60));
      window.page.paint();
    });
    const panel=page.locator('#suppliers');
    await panel.locator('.gateway-supplier-row').click();
    await panel.getByLabel('设置模型',{exact:true}).waitFor();
    await panel.getByLabel('接入密钥',{exact:true}).selectOption({index:1});
    await panel.getByLabel('设置模型',{exact:true}).waitFor();

    for(const label of ['图片输入','上下文窗口','最大输出','压缩阈值','推理思考','每个账号的并发上限','价格倍率'])
      assert.ok(await panel.getByLabel(label,{exact:true}).isVisible(),label+' must be present');

    await panel.getByLabel('上下文窗口',{exact:true}).fill('1M');
    await panel.getByLabel('最大输出',{exact:true}).fill('128K');
    await panel.getByLabel('压缩阈值',{exact:true}).fill('500k');
    await panel.getByRole('button',{name:'保存模型设置',exact:true}).click();
    await page.waitForFunction(()=>calls.some(c=>c.op==='save_policy'));
    assert.deepEqual(await page.evaluate(()=>calls.find(c=>c.op==='save_policy').policy),
      {context_window:1000000,max_output_tokens:128000,auto_compact_token_limit:500000});

    await panel.getByLabel('上下文窗口',{exact:true}).fill('n/a');
    await panel.getByRole('button',{name:'保存模型设置',exact:true}).click();
    assert.equal(await page.evaluate(()=>calls.filter(c=>c.op==='save_policy').length),1,'invalid tokens must never be submitted');

    await panel.getByLabel('上下文窗口',{exact:true}).fill('400K');
    await page.evaluate(()=>window.page.paint());
    assert.equal(await panel.getByLabel('上下文窗口',{exact:true}).inputValue(),'400K','polling must preserve model draft');

    await panel.getByLabel('推理思考',{exact:true}).selectOption('high');
    await panel.getByRole('button',{name:'保存模型设置',exact:true}).click();
    await page.waitForFunction(()=>calls.some(c=>c.op==='save_policy'&&c.policy.reasoning==='high'));
    await panel.getByLabel('推理思考',{exact:true}).selectOption('off');
    await panel.getByRole('button',{name:'保存模型设置',exact:true}).click();
    await page.waitForFunction(()=>calls.some(c=>c.op==='save_policy'&&c.policy.reasoning==='off'));

    await panel.getByLabel('设置模型',{exact:true}).selectOption('opencode/text');
    assert.equal(await panel.getByLabel('图片输入',{exact:true}).locator('option[value="native"]').count(),0,'text bridges cannot advertise native images');
    await panel.getByLabel('图片输入',{exact:true}).selectOption('describe');
    assert.equal(await panel.getByLabel('视觉模型',{exact:true}).inputValue(),'trae/eye');
    await panel.getByRole('button',{name:'保存模型设置',exact:true}).click();
    await page.waitForFunction(()=>calls.some(c=>c.op==='save_policy'&&c.model==='opencode/text'));
    assert.deepEqual(await page.evaluate(()=>calls.find(c=>c.op==='save_policy'&&c.model==='opencode/text').policy.image),{mode:'describe',model:'trae/eye'});

    await panel.getByLabel('设置模型',{exact:true}).selectOption('trae/eye');
    await panel.getByLabel('每个账号的并发上限',{exact:true}).fill('2');
    await panel.getByLabel('同时使用的账号数',{exact:true}).fill('1');
    await panel.getByLabel('每个账号每分钟请求数',{exact:true}).fill('30');
    await panel.getByLabel('每个账号的排队上限',{exact:true}).fill('8');
    await panel.getByLabel('排队等待（秒）',{exact:true}).fill('20');
    await panel.getByLabel('添加备用',{exact:true}).selectOption('opencode/text');
    await panel.getByLabel('添加备用',{exact:true}).selectOption('pool:backup');
    await panel.getByRole('button',{name:'上移',exact:true}).nth(1).click();
    await panel.getByLabel('备用优先级',{exact:true}).selectOption('cheapest');
    await panel.getByLabel('价格倍率',{exact:true}).fill('0.8');
    await panel.getByLabel('把此来源的模型视为对我免费',{exact:true}).check();
    await panel.getByRole('button',{name:'添加时段',exact:true}).click();
    await panel.getByLabel('时段倍率',{exact:true}).fill('1.5');
    await panel.getByLabel('周日',{exact:true}).check();
    await panel.getByRole('button',{name:'保存供应商设置',exact:true}).click();
    await page.waitForFunction(()=>calls.some(c=>c.op==='save_provider'&&c.scope==='provider'));
    const rule=await page.evaluate(()=>calls.find(c=>c.op==='save_provider'&&c.scope==='provider').settings);
    assert.deepEqual(rule,{concurrency:2,accounts:1,rpm:30,queue_limit:8,queue_wait:20,fallbacks:['pool:backup','opencode/text'],
      order:'cheapest',multiplier:0.8,free:true,periods:[{start:'22:00',end:'06:00',factor:1.5,days:[7]}]});

    await page.evaluate(()=>{
      window.pricing=gatewayProvider.create((op,fields)=>window.__TAURI__.core.invoke('client_gateway',{op,...fields}));
      $('pricing').append(window.pricing.render({model:'trae/eye',scope:'model',keyId:'primary',models:window.models}));
    });
    const pricing=page.locator('#pricing');
    await pricing.getByLabel('为这个模型设置单价',{exact:true}).check();
    await pricing.getByLabel('输入单价',{exact:true}).fill('2');
    await pricing.getByLabel('输出单价',{exact:true}).fill('4');
    await pricing.getByLabel('缓存读取单价',{exact:true}).fill('0.5');
    await pricing.getByRole('button',{name:'保存模型单价',exact:true}).click();
    await page.waitForFunction(()=>calls.some(c=>c.op==='save_provider'&&c.scope==='model'));
    assert.deepEqual(await page.evaluate(()=>calls.find(c=>c.op==='save_provider'&&c.scope==='model').settings),
      {input:2,output:4,cache_read:0.5,cache_write:null});
    assert.match(await pricing.innerText(),/缺用量 1 · 缺单价 1/);

    const evidence=path.join(root,'.agent-sync','gateway-ui-evidence');
    fs.mkdirSync(evidence,{recursive:true});
    await panel.screenshot({path:path.join(evidence,'supplier-settings.png'),fullPage:true});
    assert.deepEqual(errors,[]);
    console.log(JSON.stringify({result:'PASS',checks:'capability inheritance, native transport, token parsing, reasoning levels, provider rules, price payload, draft preservation',evidence}));
  } finally {await browser.close();await new Promise(resolve=>server.close(resolve));}
})().catch(e=>{console.error(e);process.exitCode=1;});
