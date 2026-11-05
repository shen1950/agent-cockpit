/* 模型反代引导：供应商 → 本地反代 → 下游客户端。
   所有配置写入都交给已有组/密钥编辑器，向导本身不改动账号。 */
(() => {
  const wizard = { step: 1, model: '', keyId: '', client: '', error: '', sourcesOpen: false };
  // Keep this node across state polling and step changes so unsaved settings survive.
  let clientHost = null;
  const sources = [
    ['qoder-cn', 'Qoder CN'], ['qoder', 'Qoder'], ['workbuddy', 'WorkBuddy'],
    ['workbuddy-intl', 'WorkBuddy 国际版'], ['traework-cn', 'TRAE CN'],
    ['opencode', 'OpenCode'], ['astudio', 'AStudio'], ['minimax', 'MiniMax Code'],
    ['qwenwork', '千问办公'], ['catpaw', '妙手'],
  ];
  const update = () => renderGuide(S.data);
  const go = n => { wizard.step = n; wizard.error = ''; update(); };
  const actions = (...buttons) => el('div', { class: 'guide-actions' }, ...buttons);
  const prev = n => btn('上一步', 'sm', () => go(n));
  const next = (n, text) => btn(text, 'primary sm', () => go(n));
  const jump = id => document.getElementById(id)?.scrollIntoView({ block: 'start', behavior: 'smooth' });
  function appDetail(id) {
    S.cur = id;
    S.view = 'agents';
    render();
    toast('完成来源设置后，返回「模型反代」继续', true);
  }
  function drawSources(body, d) {
    body.appendChild(pxNote('每个提供模型的账号或 API Key 就是一行供应商。在「供应商」列表启用或停用它，点进去设置模型能力、调度和计价。选择供应商不会改变任何客户端。'));
    const providers = window.supplierPage?.providers || [];
    const list = el('div', { class: 'guide-summary' });
    for (const provider of providers) {
      list.appendChild(el('div', {}, el('b', {}, provider.name || '未命名供应商'),
        ` · ${provider.off ? '已停用' : '已启用'} `));
    }
    if (!providers.length) body.appendChild(pxNote('还没有供应商。先在账号页或密钥页登记来源，再回到供应商列表启用。', true));
    else body.appendChild(list);
    body.appendChild(actions(btn('打开供应商列表', 'primary sm', () => jump('proxy-suppliers'))));
    const details = el('details', { class: 'guide-client' }, el('summary', {}, '查看支持的模型来源与账号状态'));
    details.open = wizard.sourcesOpen;
    details.addEventListener('toggle', () => { wizard.sourcesOpen = details.open; });
    const grid = el('div', { class: 'guide-agents' });
    const detected = guideAgents(d);
    for (const [id, name] of sources) {
      const app = (d.apps || []).find(a => a.id === id && a.installed !== false);
      const info = detected.find(a => a.app === id);
      const bridgeOnly = id === 'opencode' && !app && !!info;
      const item = el('div', { class: 'guide-agent' },
        el('div', { class: 'guide-agenthead' }, el('b', {}, name),
          chip(app ? (info?.blocked ? '受限' : '已检测') : (bridgeOnly ? (info.usable ? '桥可用' : '桥已配置') : '未检测到'), info?.blocked ? 'expired' : '')),
        el('div', { class: 'guide-agentdetail' }, info?.detail || (app ? '在来源详情中查看账号和模型' : '安装或扫描后可查看')));
      if (app) item.appendChild(actions(btn('查看来源详情', 'sm', () => appDetail(id))));
      grid.appendChild(item);
    }
    details.appendChild(pxNote('这里列的是驾驶舱已有适配的模型来源，不是将要使用反代的客户端。支持情况和账号状态请到供应商详情查看；API 台账中的 Key 也会作为供应商列出。'));
    details.appendChild(grid);
    body.appendChild(details);
    body.appendChild(actions(next(2, '下一步：选择模型')));
  }
  function drawModels(body) {
    body.appendChild(pxNote('选择或输入下游客户端要请求的模型 ID。下面的目录汇总所有启用供应商，并非针对某一把接入密钥验证；能否调用还取决于来源、账号和密钥配置。'));
    const state = S.proxyPage;
    const groups = state.models?.groups || [];
    const select = el('select', { class: 'px-select', aria: { label: '接入密钥' } });
    select.appendChild(el('option', { value: '' }, '从已读取的模型目录选择（也可手动输入）'));
    for (const group of groups) {
      const optgroup = el('optgroup', { label: PX_OWNER_NAME[group.owner] || group.owner });
      for (const model of group.models || []) optgroup.appendChild(el('option', { value: model.id }, model.id));
      select.appendChild(optgroup);
    }
    select.value = wizard.model;
    select.addEventListener('change', () => { if (select.value) wizard.model = select.value; update(); });
    body.appendChild(el('div', { class: 'px-modelrow' }, el('span', { class: 'px-k' }, '模型目录'), select,
      btn(state.modelsLoading ? '读取中…' : '读取 / 刷新模型目录', 'sm', () => loadProxyModels())));
    if (state.models?.error) body.appendChild(pxNote(`读取失败：${state.models.error}`, true));
    else if (state.models && !groups.length) body.appendChild(pxNote('当前没有列出模型；检查启用的供应商、账号和本地桥，或手动输入模型 ID。'));
    const input = el('input', { class: 'px-select', type: 'text', placeholder: '例如 trae/glm-5.3-flash' });
    input.value = wizard.model;
    input.addEventListener('input', () => { wizard.model = input.value.trim(); });
    body.appendChild(el('div', { class: 'px-modelrow' }, el('span', { class: 'px-k' }, '模型 ID'), input));
    body.appendChild(pxNote('优先复制目录中的完整 ID：例如 TRAE CN 使用 trae/ 前缀。各来源的探测和模型测试可在其详情页进行；目录不代表已经通过请求测试。'));
    body.appendChild(actions(prev(1), next(3, '下一步：启动与密钥')));
  }
  function drawService(body, d) {
    body.appendChild(pxNote('启动只是开放本机反代接口，不会接管或修改任何客户端。下游客户端只有配置了此地址并发起请求才会使用它。'));
    body.appendChild(actions(pxRunBtn(d), btn('生成接入密钥', 'sm', () => openDistModal(null)), btn('查看密钥列表', 'sm', () => jump('proxy-keys'))));
    const keys = (d.proxy.dist || []).filter(k => k.enabled !== false);
    const select = el('select', { class: 'px-select' });
    select.appendChild(el('option', { value: '' }, '选择已有接入密钥'));
    for (const k of keys) select.appendChild(el('option', { value: k.id }, k.name || '未命名密钥'));
    if (!keys.some(k => k.id === wizard.keyId)) wizard.keyId = keys.length === 1 ? keys[0].id : '';
    select.value = wizard.keyId;
    select.addEventListener('change', () => { wizard.keyId = select.value; wizard.error = ''; update(); });
    body.appendChild(el('div', { class: 'px-modelrow' }, el('span', { class: 'px-k' }, '接入密钥'), select));
    if (wizard.error) body.appendChild(pxNote(wizard.error, true));
    body.appendChild(pxNote('已有密钥可在密钥列表中编辑允许接入的供应商。当前版本对未识别的密钥会回退到全部启用供应商，请勿将其视为严格访问控制。'));
    body.appendChild(actions(prev(2), next(4, '下一步：接入本地网关')));
  }
  function drawClient(body, d) {
    const clients = d.apps || [];
    if (!clients.some(a => a.id === wizard.client)) {
      wizard.client = clients.find(a => a.id === 'codex' && a.installed)?.id || clients.find(a => a.installed)?.id || clients[0]?.id || '';
    }
    const select = el('select', { class: 'px-select', aria: { label: '接入 Agent' } });
    for (const a of clients) select.appendChild(el('option', { value: a.id }, `${a.name || a.id}${a.installed === false ? '（未检测到）' : ''}`));
    select.value = wizard.client;
    select.addEventListener('change', () => { wizard.client = select.value; update(); });
    body.appendChild(el('div', { class: 'gateway-destination' }, el('label', {}, '接入 Agent', select),
      el('p', { class: 'muted' }, '选择使用网关的 Agent，然后设置接入密钥、模型、图片能力和上下文。这里与各 Agent 页共用同一套接入设置。')));
    if (!clientHost) clientHost = el('section', { id: 'proxy-gateway-client', class: 'gateway-client gateway-client-embedded', aria: { label: '所选 Agent 的网关设置' } });
    body.appendChild(clientHost);
    if (!clients.length) body.appendChild(pxNote('尚未读取 Agent 列表，请刷新驾驶舱。'));
    body.appendChild(actions(prev(3), btn('查看实时路由', 'sm', () => jump('proxy-routes'))));
  }
  renderGuide = function (d) {
    const host = $('proxy-guide');
    if (!host || !d?.proxy) return;
    clear(host);
    const base = proxyBase(d);
    const card = el('section', { class: 'panel guide-card' });
    card.appendChild(el('div', { class: 'guide-head' },
      el('div', { class: 'guide-title' }, el('b', {}, '模型反代接入'),
        el('span', { class: 'guide-sub' }, '供应商 → 本地反代 → 下游客户端')),
      el('span', { class: 'sp' }), chip(d.proxy.running ? '反代运行中' : '反代未启动', d.proxy.running ? 'swap' : 'expired'),
      pxRunBtn(d), el('code', { class: 'mono guide-base', title: base }, base),
      btn('复制地址', 'sm', () => copyText(base))));
    card.appendChild(proxyRiskNote());
    const labels = ['配置供应商', '选择模型', '启动与密钥', '接入本地网关'];
    const progress = el('div', { class: 'guide-progress' });
    labels.forEach((label, index) => {
      const n = index + 1;
      progress.appendChild(el('button', { type: 'button', class: `guide-step${wizard.step === n ? ' active' : ''}${wizard.step > n ? ' done' : ''}`,
        aria: { label: `第 ${n} 步：${label}` },
        onclick: () => go(n), title: `第 ${n} 步：${label}` },
        el('span', { class: 'guide-dot' }, wizard.step > n ? '✓' : n), el('span', { class: 'guide-label' }, label)));
    });
    card.appendChild(progress);
    const body = el('div', { class: 'guide-body' });
    if (wizard.step === 1) drawSources(body, d);
    else if (wizard.step === 2) drawModels(body);
    else if (wizard.step === 3) drawService(body, d);
    else drawClient(body, d);
    card.appendChild(body);
    host.appendChild(card);
    if (wizard.step === 4 && wizard.client) {
      gatewayClient.mount(clientHost, d, { app: wizard.client, keyId: wizard.keyId, model: wizard.model });
    }
  };
})();
