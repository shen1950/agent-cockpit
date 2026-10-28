/* 模型反代引导：来源 → 上游组 → 本地反代 → 下游客户端。
   所有配置写入都交给已有组/密钥编辑器，向导本身不改动账号。 */
(() => {
  const wizard = { step: 1, model: '', keyId: '', key: '', revealing: false, error: '' };
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
    body.appendChild(pxNote('把提供模型的账号或 API Key 加入上游组。一个组可以同时勾选多个来源、多个账号；多个组也可以同时启用。选择来源不会改变任何客户端。'));
    const pools = d.proxy.pools || [];
    if (pools.length) {
      const list = el('div', { class: 'guide-summary' });
      for (const group of pools) {
        const desc = (group.labels || []).join('、') || `${(group.members || []).length} 个成员`;
        list.appendChild(el('div', {}, el('b', {}, group.name || '未命名组'),
          ` · ${group.enabled === false ? '已停用' : '已启用'} · ${desc} `,
          btn('编辑成员', 'sm', () => openPoolModal(group), '勾选或取消多个来源')));
      }
      body.appendChild(list);
    } else body.appendChild(pxNote('还没有上游组，先新建一个并勾选来源。', true));
    body.appendChild(actions(
      btn('新建上游组（可多选来源）', 'primary sm', () => openPoolModal(null)),
      btn('查看全部上游组', 'sm', () => jump('proxy-groups'))));
    const details = el('details', { class: 'guide-client' }, el('summary', {}, '查看支持的模型来源与账号状态'));
    const grid = el('div', { class: 'guide-agents' });
    const detected = guideAgents(d);
    for (const [id, name] of sources) {
      const app = (d.apps || []).find(a => a.id === id && a.installed !== false);
      const info = detected.find(a => a.app === id);
      const item = el('div', { class: 'guide-agent' },
        el('div', { class: 'guide-agenthead' }, el('b', {}, name),
          chip(app ? (info?.blocked ? '受限' : '已检测') : '未检测到', info?.blocked ? 'expired' : '')),
        el('div', { class: 'guide-agentdetail' }, info?.detail || (app ? '在来源详情中查看账号和模型' : '安装或扫描后可查看')));
      if (app) item.appendChild(actions(btn('查看来源详情', 'sm', () => appDetail(id))));
      grid.appendChild(item);
    }
    details.appendChild(pxNote('这里列的是驾驶舱已有适配的模型来源，不是将要使用反代的客户端。支持情况和账号状态请看各来源详情；API 台账中的 Key 也可以加入上游组。'));
    details.appendChild(grid);
    body.appendChild(details);
    body.appendChild(actions(next(2, '下一步：选择模型')));
  }
  function drawModels(body) {
    body.appendChild(pxNote('选择或输入下游客户端要请求的模型 ID。下面的目录汇总所有启用上游组，并非针对某一把接入密钥验证；能否调用还取决于来源、账号和密钥配置。'));
    const state = S.proxyPage;
    const groups = state.models?.groups || [];
    const select = el('select', { class: 'px-select' });
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
    else if (state.models && !groups.length) body.appendChild(pxNote('当前没有列出模型；检查上游组、账号和本地桥，或手动输入模型 ID。'));
    const input = el('input', { class: 'px-select', type: 'text', placeholder: '例如 trae/glm-5.3-flash' });
    input.value = wizard.model;
    input.addEventListener('input', () => { wizard.model = input.value.trim(); });
    body.appendChild(el('div', { class: 'px-modelrow' }, el('span', { class: 'px-k' }, '模型 ID'), input));
    body.appendChild(pxNote('优先复制目录中的完整 ID：例如 TRAE CN 使用 trae/ 前缀。各来源的探测和模型测试可在其详情页进行；目录不代表已经通过请求测试。'));
    body.appendChild(actions(prev(1), next(3, '下一步：启动与密钥')));
  }
  async function revealKey() {
    if (!wizard.keyId || wizard.revealing) return;
    const id = wizard.keyId;
    wizard.revealing = true; wizard.error = ''; update();
    try {
      const result = await invoke('action', { payload: { action: 'proxy_dist_reveal', id } });
      if (wizard.keyId === id) {
        if (result.ok && result.msg) wizard.key = result.msg;
        else wizard.error = result.msg || '无法读取接入密钥';
      }
    } catch (error) { wizard.error = String(error); }
    wizard.revealing = false; update();
  }
  function drawService(body, d) {
    body.appendChild(pxNote('启动只是开放本机反代接口，不会接管或修改任何客户端。下游客户端只有配置了此地址并发起请求才会使用它。'));
    body.appendChild(actions(pxRunBtn(d), btn('生成接入密钥', 'sm', () => openDistModal(null)), btn('查看密钥列表', 'sm', () => jump('proxy-keys'))));
    const keys = (d.proxy.dist || []).filter(k => k.enabled !== false);
    const select = el('select', { class: 'px-select' });
    select.appendChild(el('option', { value: '' }, '选择已有接入密钥'));
    for (const k of keys) select.appendChild(el('option', { value: k.id }, k.name || '未命名密钥'));
    if (!keys.some(k => k.id === wizard.keyId)) { wizard.keyId = ''; wizard.key = ''; }
    select.value = wizard.keyId;
    select.addEventListener('change', () => { wizard.keyId = select.value; wizard.key = ''; wizard.error = ''; update(); });
    const reveal = btn(wizard.revealing ? '读取中…' : '读取所选密钥', 'sm', revealKey);
    reveal.disabled = !wizard.keyId || wizard.revealing;
    body.appendChild(el('div', { class: 'px-modelrow' }, el('span', { class: 'px-k' }, '接入密钥'), select, reveal));
    if (wizard.key) body.appendChild(pxNote('已读取密钥，可在下一步复制配置。'));
    if (wizard.error) body.appendChild(pxNote(wizard.error, true));
    body.appendChild(pxNote('已有密钥可在密钥列表中编辑关联的上游组。当前版本对未识别的密钥会回退到全部启用组，请勿将其视为严格访问控制。'));
    body.appendChild(actions(prev(2), next(4, '下一步：配置客户端')));
  }
  function drawClient(body, d) {
    body.appendChild(pxNote('选择你想使用模型的下游客户端，在它的 OpenAI 兼容提供商设置中填写下列信息。来源和客户端是独立选择：使用 TRAE CN 作为来源，不要求填回 TRAE。'));
    body.appendChild(pxRow('Base URL', proxyBase(d), [btn('复制', 'sm', () => copyText(proxyBase(d)))]));
    body.appendChild(pxRow('模型 ID', wizard.model || '未选择或输入', wizard.model ? [btn('复制', 'sm', () => copyText(wizard.model))] : []));
    body.appendChild(pxRow('API Key', wizard.key ? '已读取（点击复制）' : '请在上一步读取接入密钥', wizard.key ? [btn('复制', 'sm', () => copyText(wizard.key))] : []));
    const copy = btn('复制完整配置', 'primary sm', () => copyText(`Base URL: ${proxyBase(d)}\nModel: ${wizard.model}\nAPI Key: ${wizard.key}`));
    copy.disabled = !wizard.key || !wizard.model;
    body.appendChild(actions(prev(3), copy));
    body.appendChild(pxNote('这里提供通用配置，不会自动写入客户端。客户端支持的协议和配置入口各不相同；配置完成后请在所选客户端发起请求，并查看下方请求日志验证。'));
    if (!d.proxy.running) body.appendChild(pxNote('反代当前未启动，客户端暂时无法连接。返回上一步启动。', true));
  }
  renderGuide = function (d) {
    const host = $('proxy-guide');
    if (!host || !d?.proxy) return;
    clear(host);
    const base = proxyBase(d);
    const card = el('section', { class: 'panel guide-card' });
    card.appendChild(el('div', { class: 'guide-head' },
      el('div', { class: 'guide-title' }, el('b', {}, '模型反代接入'),
        el('span', { class: 'guide-sub' }, '模型来源 → 上游组 → 本地反代 → 下游客户端')),
      el('span', { class: 'sp' }), chip(d.proxy.running ? '反代运行中' : '反代未启动', d.proxy.running ? 'swap' : 'expired'),
      pxRunBtn(d), el('code', { class: 'mono guide-base', title: base }, base),
      btn('复制地址', 'sm', () => copyText(base))));
    const labels = ['配置上游来源', '选择模型', '启动与密钥', '配置客户端'];
    const progress = el('div', { class: 'guide-progress' });
    labels.forEach((label, index) => {
      const n = index + 1;
      progress.appendChild(el('button', { type: 'button', class: `guide-step${wizard.step === n ? ' active' : ''}${wizard.step > n ? ' done' : ''}`,
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
  };
})();
