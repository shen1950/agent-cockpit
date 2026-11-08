/* Provider scheduling and local cost estimates share the key-scoped gateway catalog. */
const gatewayProvider = (() => {
  const quotas = [
    ['concurrency','每个账号的并发上限',1000,'超出时排队，0 或留空表示不限。'],
    ['accounts','同时使用的账号数',1000,'按上游列表取前几个可用账号；不可用时由后续账号接替。'],
    ['rpm','每个账号每分钟请求数',1000000,'超出时等待空位，受排队等待限制；RPM 等待最长 2 分钟。'],
    ['queue_limit','每个账号的排队上限',10000,'队列已满时拒绝新请求，0 或留空表示不限。'],
    ['queue_wait','排队等待（秒）',3600,'超过等待时间时返回错误，0 或留空表示一直等。'],
  ];
  const numeric = (raw, label, max, integer = false, fallback = null) => {
    if (String(raw ?? '').trim() === '') return fallback;
    const n = Number(raw);
    if (!Number.isFinite(n) || n < 0 || n > max || (integer && !Number.isInteger(n)))
      throw Error(label + '需要填写 0–' + max.toLocaleString() + (integer ? ' 的整数。' : ' 的数值。'));
    return n;
  };
  const hint = text => el('p', {class:'gateway-hint'}, text);
  function field(label, value, attrs = {}, oninput) {
    const input = el('input', {class:'px-input', type:'number', min:0, step:'any', value:value ?? '', ...attrs, aria:{label}});
    if (oninput) input.oninput = () => oninput(input.value);
    return {input, node:el('label', {class:'gateway-field'}, el('span',{},label), input)};
  }
  function select(label, choices, value, change) {
    const input=el('select',{class:'px-select',aria:{label}});
    for (const [id,name] of choices) input.append(el('option',{value:id},name));
    input.value=value;
    input.onchange=()=>change(input.value);
    return el('label',{class:'gateway-field'},el('span',{},label),input);
  }
  function create(call) {
    const editors = new Map();
    function render({model, scope, keyId, models}) {
      const id=[keyId,model,scope].join('\0');
      if (editors.has(id)) return editors.get(id);
      const box=el('section',{class:'gateway-provider',aria:{label:scope==='provider'?'供应商调度与计价':'模型计价'}});
      editors.set(id,box);
      let data=null, draft=null, busy=false;
      const request=(op,fields={})=>call(op,{keyId,model,scope,...fields});
      const message=el('p',{class:'gateway-message',aria:{live:'polite'}});
      function lock(value) {busy=value; box.querySelectorAll('button,input,select').forEach(e=>{e.disabled=value || e.dataset.unavailable==='true';});}
      function usage() {
        const s=data.summary || {};
        const row=el('div',{class:'gateway-cost-summary'},el('strong',{},'估算 $'+Number(s.usd || 0).toFixed(6)),
          el('span',{},`${s.priced || 0} / ${s.requests || 0} 次已计价`),
          el('span',{},`缺用量 ${s.missing_usage || 0} · 缺单价 ${s.missing_price || 0}`));
        return el('div',{},row,hint('仅按网关记录的 token 估算美元费用，不等同于供应商账单。缺失的数据单列，不算作免费；修改单价后，历史估算也会更新。'));
      }
      async function load() {
        lock(true); message.textContent='正在读取设置…';
        try {
          data=await request('provider_info');
          draft=scope==='provider' ? {...data.rule, fallbacks:[...(data.rule?.fallbacks || [])], periods:(data.rule?.periods || []).map(p=>({...p,days:[...(p.days || [])]}))}
            : {custom:!!data.price,...(data.price || {})};
          paint(); message.textContent='';
        } catch(e) {
          message.textContent=String(e);
          if (!draft) {clear(box);box.append(message,btn('重试读取','sm',load));}
        }
        finally {lock(false);}
      }
      async function save() {
        if (busy) return;
        try {
          let settings;
          if (scope==='provider') {
            settings={...draft};
            for (const [key,title,max] of quotas) settings[key]=numeric(draft[key],title,max,true,0);
            settings.multiplier=numeric(draft.multiplier,'价格倍率',10000);
            settings.periods=draft.periods.map(p=>{
              if (!/^([01]\d|2[0-3]):[0-5]\d$/.test(p.start) || !/^([01]\d|2[0-3]):[0-5]\d$/.test(p.end) || p.start===p.end)
                throw Error('每个时段请填写不同的起止时间（HH:MM）。');
              const factor=numeric(p.factor,'时段倍率',10000);
              if (factor===null) throw Error('请填写时段倍率。');
              return {...p,factor};
            });
          } else if (draft.custom) {
            settings={};
            for (const [key,title] of [['input','输入单价'],['output','输出单价'],['cache_read','缓存读取单价'],['cache_write','缓存写入单价']])
              settings[key]=numeric(draft[key],title,1000000);
            if (settings.input===null || settings.output===null) throw Error('自定义计价需要同时填写输入和输出单价。');
          } else settings=null;
          lock(true); message.textContent='正在保存…';
          await request('save_provider',{settings});
          data=await request('provider_info');
          paint(); message.textContent='已保存，新请求立即使用这些设置。';
        } catch(e) {message.textContent=String(e);}
        finally {lock(false);}
      }
      function scheduling() {
        box.append(el('h3',{},'供应商调度'),hint('此来源的所有模型共用账号调度和计价规则。只使用当前接入密钥授权的账号、模型与备用组。'));
        const grid=el('div',{class:'gateway-field-grid'});
        for (const [key,title,max,note] of quotas) {
          const f=field(title,draft[key] || '',{step:1,max,placeholder:'不限'},v=>{draft[key]=v;});
          grid.append(el('div',{},f.node,hint(note)));
        }
        box.append(grid);
        const routes=el('div',{class:'gateway-fallbacks'});
        function routeList() {
          clear(routes);
          const available=new Map([...models.map(m=>[m.id,m.id]),...(data.groups || []).map(g=>['pool:'+g.id,'上游组 · '+g.name])]);
          draft.fallbacks.forEach((id,i)=>{
            const up=btn('上移','sm',()=>{[draft.fallbacks[i-1],draft.fallbacks[i]]=[draft.fallbacks[i],draft.fallbacks[i-1]];routeList();});
            const down=btn('下移','sm',()=>{[draft.fallbacks[i+1],draft.fallbacks[i]]=[draft.fallbacks[i],draft.fallbacks[i+1]];routeList();});
            up.disabled=i===0;up.dataset.unavailable=String(i===0);
            down.disabled=i===draft.fallbacks.length-1;down.dataset.unavailable=String(i===draft.fallbacks.length-1);
            routes.append(el('div',{class:'gateway-fallback'},el('span',{class:'gateway-route-number'},i+1),
              el('span',{},available.get(id) || id+'（当前密钥未授权，跳过）'),up,down,
              btn('移除','sm',()=>{draft.fallbacks.splice(i,1);routeList();})));
          });
          if (!draft.fallbacks.length) routes.append(hint('尚未设置备用模型或上游组。'));
          const choices=[['','添加备用模型或上游组'],...[...available].filter(([id])=>id!==model && !draft.fallbacks.includes(id))];
          routes.append(select('添加备用',choices,'',id=>{
            if (!id) return;
            if (draft.fallbacks.length>=24) {message.textContent='最多添加 24 个备用项。';return;}
            draft.fallbacks.push(id);routeList();
          }));
        }
        routeList();
        box.append(el('h4',{},'备用顺序'),routes,
          select('备用优先级',[['listed','按列表顺序'],['free_first','免费优先'],['cheapest','价格最低优先']],draft.order || 'listed',v=>{draft.order=v;}),
          hint('来源额度用完、限流或不可用时尝试备用；只在回复内容开始前切换。未授权的备用项会跳过。免费和价格优先仅调整备用顺序。'));
        box.append(el('h3',{},'供应商计价'));
        const multiplier=field('价格倍率',draft.multiplier,{max:10000,placeholder:'1（目录价格）'},v=>{draft.multiplier=v;});
        const free=el('input',{type:'checkbox',checked:!!draft.free});free.onchange=()=>{draft.free=free.checked;};
        box.append(multiplier.node,hint('目录单价乘以此倍率；单独设置过单价的模型保留其自定义单价。'),
          el('label',{class:'gateway-check'},free,'把此来源的模型视为对我免费'),
          hint('用于“免费优先”和“价格最低优先”的排序。费用统计仍按单价记录。'));
        const periods=el('div',{class:'gateway-periods'});
        function periodList() {
          clear(periods);
          draft.periods.forEach((p,i)=>{
            const start=field('开始时间',p.start,{type:'time',min:undefined,step:undefined},v=>{p.start=v;});
            const end=field('结束时间',p.end,{type:'time',min:undefined,step:undefined},v=>{p.end=v;});
            const factor=field('时段倍率',p.factor,{max:10000},v=>{p.factor=v;});
            const days=el('div',{class:'gateway-weekdays'});
            ['一','二','三','四','五','六','日'].forEach((day,j)=>{
              const check=el('input',{type:'checkbox',checked:p.days.includes(j+1)});
              check.onchange=()=>{p.days=check.checked ? [...p.days,j+1] : p.days.filter(n=>n!==j+1);};
              days.append(el('label',{},check,'周'+day));
            });
            periods.append(el('div',{class:'gateway-period'},el('div',{class:'gateway-controls'},
              el('b',{},String(i+1)),start.node,end.node,factor.node,btn('移除时段','sm',()=>{draft.periods.splice(i,1);periodList();})),days));
          });
        }
        periodList();
        box.append(el('h4',{},'时段倍率'),periods,btn('添加时段','sm',()=>{
          if (draft.periods.length>=32) {message.textContent='最多添加 32 个时段。';return;}
          draft.periods.push({start:'22:00',end:'06:00',factor:1,days:[]});periodList();
        }),hint('使用运行网关电脑的本地时间；不选星期表示每天。跨午夜时归属开始那天。按顺序使用首个匹配时段，并叠加供应商倍率；模型自定义单价也应用时段倍率。'));
      }
      function pricing() {
        box.append(el('h3',{},'模型计价'),hint('单价单位：美元 / 百万 token。可为代理模型设置实际单价，或沿用 models.dev 目录中同名厂商模型的价格。'));
        const custom=el('input',{type:'checkbox',checked:draft.custom});custom.onchange=()=>{draft.custom=custom.checked;paint();};
        box.append(el('label',{class:'gateway-check'},custom,'为这个模型设置单价'));
        if (draft.custom) {
          const grid=el('div',{class:'gateway-field-grid'});
          for (const [key,title] of [['input','输入单价'],['output','输出单价'],['cache_read','缓存读取单价'],['cache_write','缓存写入单价']])
            grid.append(field(title,draft[key],{max:1000000,placeholder:key.startsWith('cache')?'沿用输入单价':'必填'},v=>{draft[key]=v;}).node);
          box.append(grid);
        }
        const rate=data.effective;
        box.append(hint(rate ? `当前生效：输入 $${rate.input} · 输出 $${rate.output} / 百万 token` : '暂无可匹配的单价；此模型请求会计入“缺单价”，不会记为零费用。'));
        box.append(btn('更新公开模型与价格目录','sm',async()=>{
          if (busy) return;
          lock(true);message.textContent='正在更新 models.dev 目录…';
          try {
            const r=await request('refresh_prices');data=await request('provider_info');paint();
            message.textContent=`已更新 ${r.count} 条目录记录，未保存的单价草稿已保留。`;
          } catch(e){message.textContent=String(e);}finally{lock(false);}
        }),hint('公开目录仅按准确的模型 ID 匹配，不用相似名字猜单价。'));
      }
      function paint() {
        clear(box);
        if (!draft) {box.append(message);return;}
        if (scope==='provider') scheduling();else pricing();
        box.append(usage(),el('div',{class:'gateway-controls'},btn(scope==='provider'?'保存供应商设置':'保存模型单价','primary sm',save),
          btn('重新读取设置','sm',load)),message);
      }
      box.append(message);load();
      return box;
    }
    return {render, clear(){editors.clear();}};
  }
  return {create};
})();
