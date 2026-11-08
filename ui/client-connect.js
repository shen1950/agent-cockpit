/* Loaded before app.js. Event handlers use the initialized desktop bridge. */
const cockpitSessions = (() => {
  let origin = 'home', agent = '', session = '', request = 0;
  async function connect() {
    const seq = ++request;
    const frame = $('sessions-frame'), status = $('sessions-notice');
    frame.hidden = true; status.hidden = false;
    status.textContent = '正在连接本机 AgentFind…';
    try {
      const port = await invoke('agentfind_port');
      if (seq !== request || S.view !== 'sessions') return;
      if (!Number.isInteger(port) || port < 1024 || port > 65535) throw Error('服务端口无效');
      const url = new URL('http://127.0.0.1:' + port);
      url.searchParams.set('embed', 'cockpit');
      url.searchParams.set('theme', document.documentElement.dataset.theme || 'light');
      if (agent) url.searchParams.set('agent', agent);
      if (session) url.searchParams.set('sid', session);
      frame.src = url.href; frame.hidden = false; status.hidden = true;
    } catch (error) { if (seq === request) status.textContent = String(error); }
  }
  function open(scope = '', sid = '') {
    if (S.view !== 'sessions') origin = S.view;
    agent = scope; session = sid; S.view = 'sessions'; render(); connect();
  }
  return {open, bind() {
    $('btn-agent-sessions').onclick = () => open(S.cur);
    $('sessions-retry').onclick = connect;
    $('sessions-back').onclick = () => { request++; $('sessions-frame').removeAttribute('src'); S.view = origin; render(); };
  }};
})();

const gatewayClient = (() => {
  const panels = new Map();
  function createPanel() {
    let host, d, app, accounts=[], keys=[], state, provider, signature='', generation=0, seeds={};
    const alive = snapshot => snapshot === state;
    const call = (op, fields={}) => invoke('client_gateway',{app:state.app,name:state.name,op,keyId:state.keyId,model:state.model,...fields});
    function selector(label, choices, value, change) {
      const input = el('select', {class:'px-select',aria:{label}});
      choices.forEach(([id,title]) => input.append(el('option',{value:id},title)));
      input.value = value; input.disabled = state.busy;
      input.onchange = () => change(input.value);
      return el('label',{},label,input);
    }
    const proxyPage = () => { S.view = 'proxy'; renderApp(); };
    function renderApp() { globalThis.render(); }
    function manualPanel() {
      const block=el('div',{class:'gateway-manual'});
      const base='http://127.0.0.1:'+(d.proxy?.port || 8788)+'/v1';
      block.append(el('p',{},'在此 Agent 的自定义供应商中选择 OpenAI 兼容接口，并填入以下配置。'),
        el('div',{class:'gateway-controls'},el('code',{},base),btn('复制地址','sm',()=>copyText(base))),
        selector('接入模型',[['','选择模型'],...state.models.map(m=>[m,m])],state.model,m=>{
          state.model=m;paint();
        }));
      async function copyConfig(full) {
        const snapshot=state;
        try {
          const res=await invoke('action',{payload:{action:'proxy_dist_reveal',id:state.keyId}});
          if (!res.ok) throw Error(res.msg || '读取密钥失败');
          if (!alive(snapshot)) return;
          await copyText(full ? `Base URL: ${base}\nModel: ${state.model}\nAPI Key: ${res.msg}` : res.msg);
        } catch(e) {if (alive(snapshot)) {state.message=String(e);paint();}}
      }
      const key=btn('复制接入密钥','sm',()=>copyConfig(false));key.disabled=!state.keyId;
      const full=btn('复制完整配置','primary sm',()=>copyConfig(true));full.disabled=!state.keyId || !state.model;
      block.append(el('div',{class:'gateway-controls'},key,full),el('p',{},'自动写入暂未适配此 Agent。手动保存后，请在客户端新建会话并验证请求；图片与上下文能力还需客户端支持读取网关目录。'));
      return block;
    }
    function paint() {
      if (!host || !state) return;
      clear(host);
      const s = state.status;
      host.append(el('header', { class: 'panelhead' },
        el('h2', {}, '接入本地网关'),
        el('span', { class: 'sub' }, '改客户端配置，让它走本机 127.0.0.1 网关'),
        el('span', { class: 'sp' }),
        chip(s?.connected ? '已接入' : s?.managed ? '待核对' : s?.supported ? '未接入' : '客户端配置', s?.connected ? 'ok' : '')));
      const row = el('div',{class:'gateway-controls'});
      if (accounts.length) row.append(selector('档案',[['','本机默认'],...accounts.map(n=>[n,n])],state.name,n=>{
        state.name=n;loadStatus();
      }));
      if (s) {
          row.append(selector('密钥',[['','选择接入密钥'],...keys.map(k=>[k.id,k.name])],state.keyId,id=>{
            state.keyId=id; state.model=''; provider.clear();loadModels();
          }));
          if (s.supported && (!s.managed || !s.single)) {
            row.append(selector('接入模型',[['',state.fetching ? '读取中…' : s.single ? '请选择模型' : '网关全部模型'],...state.models.map(m=>[m,m])],state.model,m=>{
              state.model=m; paint();
            }));
            const button=btn(s.managed ? '更新接入配置' : '一键接入','primary sm',()=>act(s.managed ? 'refresh':'connect'));
            button.disabled=state.busy || !d.proxy?.running || !state.keyId || !app.installed || state.fetching || !state.models.length || (s.single && !state.model);
            row.append(button);
          }
          if (s.managed) row.append(btn('断开并恢复','sm',()=>act('disconnect')));
      }
      if (host.id==='gateway-client') row.append(btn('网关设置','sm',proxyPage));
      row.append(btn('刷新','sm',()=>{provider.clear();loadStatus();}));
      if (state.busy) row.querySelectorAll('button,select').forEach(b=>b.disabled=true);
      host.append(el('p',{},state.message || (!s ? '正在读取本机配置…' : !s.supported ? '此客户端尚未适配自动写入。网关设置中可复制地址、密钥和模型 ID。' :
        s.managed ? (s.connected ? '当前配置使用 ' + s.model + '。' : '配置已变化，请核对后恢复。') :
        !d.proxy?.running ? '请先在网关设置中启动模型反代。' : !keys.length ? '请先在网关设置中生成接入密钥。' :
        s.single ? 'Claude 桌面版会进入第三方网关模式，使用下方所选模型。断开后恢复原档案。' :
        '选择密钥和模型后接入。读图、上下文和调度设置在模型反代页的供应商详情里。')),row);
      if (s && !s.supported) host.append(manualPanel());
      if (s?.path) host.append(el('p',{title:s.path},'配置文件　'+s.path));
      if (state.busy) host.querySelectorAll('button,select,input').forEach(b=>{if (!b.closest('.gateway-provider')) b.disabled=true;});
    }
    async function loadModels() {
      const id=state.keyId;
      const snapshot=state;
      const seq=state.loading=(state.loading || 0)+1;
      state.models=[]; state.details=[]; state.message=''; state.fetching=!!(id && d.proxy?.running); paint();
      if (!state.fetching) return;
      try {
        const res=await call('models');
        if (!alive(snapshot) || seq!==state.loading) return;
        state.models=res.models; state.details=res.details || [];
        if (!state.models.includes(state.model)) state.model=state.models.includes(state.status?.model) ? state.status.model : state.status?.single ? state.models[0] || '' : '';
        if (!state.models.length) state.message='这把密钥没有列出模型，请检查关联的上游组。';
      } catch(e) { if (alive(snapshot) && seq===state.loading) state.message=String(e); }
      finally { if (alive(snapshot) && seq===state.loading) {state.fetching=false; paint();} }
    }
    async function act(op) {
      if (state.busy) return;
      const snapshot=state;
      state.busy=true; state.message='正在处理配置…'; paint();
      try {await call(op); if (alive(snapshot)) {toast(op==='disconnect'?'已恢复客户端配置':'已写入网关配置',true); await loadStatus();}}
      catch(e) {if (alive(snapshot)) state.message=String(e);}
      finally {if (alive(snapshot)) {state.busy=false; paint();}}
    }
    async function loadStatus() {
      const snapshot=state, ticket=++generation;
      state.message='';paint();
      try {
        const s=await call('status');
        if (!alive(snapshot) || generation!==ticket) return;
        state.status=s;paint();await loadModels();
      } catch(e) {if (alive(snapshot) && generation===ticket) {state.message=String(e);paint();}}
    }
    return {mount(nextHost,data,options={},force=false) {
      const previous=host;
      host=nextHost; d=data;
      app=(d.apps || []).find(a=>a.id===options.app);
      if (!app) {host.hidden=true;state=null;signature='';generation++;return;}
      host.hidden=false;host.classList.add('gateway-client');
      accounts=['codex','codex-desktop','claude'].includes(app.id) ? (d.instances || []).filter(i=>i.app===app.id).map(i=>i.name) : [];
      keys=(d.proxy?.dist || []).filter(k=>k.enabled!==false);
      const sig=JSON.stringify([app.id,accounts,keys.map(k=>[k.id,k.name,k.pools]),app.installed,d.proxy?.running,d.proxy?.port,options.keyId,options.model]);
      if (signature===sig && !force) {
        if (previous!==host && previous) host.append(...previous.childNodes);
        return;
      }
      signature=sig;
      const seedChanged=seeds.keyId!==options.keyId || seeds.model!==options.model;
      seeds={keyId:options.keyId,model:options.model};
      if (!state || state.app!==app.id) {
        state={app:app.id,name:'',keyId:keys.some(k=>k.id===options.keyId)?options.keyId:(keys.length===1?keys[0].id:''),
          model:options.model || '',status:null,models:[],details:[],busy:false,message:'',fetching:false};
        // Capture this panel's identity; late provider reads must not target a new Agent.
        const initial=state;
        provider=gatewayProvider.create((op,fields)=>invoke('client_gateway',{app:initial.app,name:initial.name,op,keyId:initial.keyId,model:initial.model,...fields}));
        loadStatus();return;
      }
      if (!accounts.includes(state.name)) state.name='';
      if (seedChanged) {
        if (keys.some(k=>k.id===options.keyId)) state.keyId=options.keyId;
        if (options.model!==undefined) state.model=options.model;
        provider.clear();loadModels();return;
      }
      const invalidKey=!keys.some(k=>k.id===state.keyId);
      if (invalidKey) {state.keyId=keys.length===1?keys[0].id:'';provider.clear();loadStatus();return;}
      // Background status updates do not reset unsaved form values.
      if (force) {provider.clear();loadStatus();}
      else if (!d.proxy?.running) {state.models=[];state.details=[];state.loading=(state.loading || 0)+1;state.fetching=false;paint();}
      else if (!state.details.length && !state.fetching) loadModels();
      else paint();
    }};
  }
  function mount(host,d,options={},force=false) {
    if (!host || !d) return;
    const key=host.id || host;
    if (!panels.has(key)) panels.set(key,createPanel());
    panels.get(key).mount(host,d,options,force);
  }
  return {mount,render(d,force=false){mount($('gateway-client'),d,{app:S.cur},force);}};
})();
