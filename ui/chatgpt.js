/* ChatGPT desktop account UI. Secret inputs remain in the dialog's memory;
   they are never added to the generic account metadata form. */
const chatgptOfficialSessions = new Map();
let chatgptDialogCounter = 0;
const chatgptOfficialSteps = [
  ['preparing', '准备临时配置'], ['launching', '打开官方客户端'], ['waiting', '等待登录完成'],
  ['closing', '关闭官方客户端'], ['importing', '读取登录信息'], ['cleaning', '清理临时配置']
];
function chatgptOfficialPanel(app, options) {
  const cacheKey = `${app}:${options.name || 'new'}`;
  let session = chatgptOfficialSessions.get(cacheKey) || null;
  let timer, disposed = false, starting = false, lastStep = -1, notified = false, error = '', closeAfterCancel = false;
  const steps = chatgptOfficialSteps.map(([, label]) => el('li', {}, label));
  const status = el('div', { class: 'hintline', role: 'status', aria: { live: 'polite' } });
  const start = btn('开始官方登录', 'primary sm', begin);
  const retry = btn('重试本次登录', 'primary sm', async () => {
    if (!session?.id || starting) return;
    await request('chatgpt_official_retry');
  });
  const cancel = btn('取消本次登录', 'sm', async () => {
    if (await confirmCancel()) await request('chatgpt_official_cancel');
  });
  const box = el('div', { class: 'cpt-official' },
    el('ol', { class: 'cpt-login-steps', aria: { label: '官方登录进度' } }, ...steps), status,
    el('div', { class: 'ctlrow' }, start, retry, cancel));
  function active() { return starting || !!session && (session.running || chatgptOfficialSteps.some(([phase]) => phase === session.phase)); }
  function save(value) {
    session = value;
    if (session?.id) chatgptOfficialSessions.set(cacheKey, session);
    if (session?.phase === 'cancelled' || session?.phase === 'completed') chatgptOfficialSessions.delete(cacheKey);
    draw();
    options.onSession?.(session);
    if (session?.phase === 'completed' && !notified) { notified = true; options.onSuccess(session.name || options.name); }
    if (session?.phase === 'cancelled' && closeAfterCancel) options.onCloseReady?.();
  }
  function draw() {
    const current = chatgptOfficialSteps.findIndex(([phase]) => phase === session?.phase);
    if (current >= 0) lastStep = current;
    steps.forEach((node, index) => {
      const done = session?.phase === 'completed' || index < lastStep;
      node.className = done ? 'is-complete' : index === lastStep ? (error || session?.phase === 'failed' ? 'is-failed' : active() ? 'is-current' : '') : '';
      node.setAttribute('aria-current', index === lastStep && current >= 0 ? 'step' : 'false');
    });
    status.textContent = error || session?.message || (session?.phase === 'completed' ? '登录已完成，账号已保存。'
      : session?.phase === 'failed' ? '登录未完成，已读取的凭据会保留，可以重试。'
        : session?.phase === 'cancelled' ? '本次登录已取消。' : '');
    box.setAttribute('aria-busy', String(active()));
    start.hidden = !!session && session.phase !== 'cancelled'; start.disabled = starting;
    retry.textContent = session?.imported ? '重试清理临时配置' : '重试本次登录';
    retry.hidden = !(session?.phase === 'failed' && session.can_retry); retry.disabled = starting;
    cancel.textContent = session?.phase === 'failed' ? '清理临时配置并重新开始' : '取消本次登录';
    cancel.hidden = !(active() || session?.phase === 'failed' && !session.imported); cancel.disabled = starting;
    options.onBusy?.(active());
  }
  async function request(action) {
    if (!session?.id) return false;
    starting = true; error = ''; draw(); clearTimeout(timer);
    try {
      const r = await invoke('action', { payload: { action, app, session_id: session.id } });
      if (r?.session) save(r.session);
      if (!r?.ok) { error = r?.msg || r?.session?.message || '操作未完成，请重试。'; return false; }
      return true;
    } catch (e) { error = String(e); return false; }
    finally { starting = false; draw(); schedule(); }
  }
  async function begin() {
    if (starting) return;
    starting = true; error = ''; lastStep = 0; draw();
    try {
      const r = await invoke('action', { payload: { action: 'chatgpt_official_start', app,
        name: options.name || '', new_name: options.newName(), history_mode: options.historyMode() } });
      if (!r?.ok && !r?.session) { error = r?.msg || '官方登录未能开始。'; return; }
      if (r.session) save(r.session);
    } catch (e) { error = String(e); }
    finally { starting = false; draw(); schedule(); }
  }
  function schedule() {
    clearTimeout(timer);
    if (!disposed && active() && !starting) timer = setTimeout(poll, 1000);
  }
  async function poll() {
    if (disposed || !session?.id) return;
    try {
      const r = await invoke('action', { payload: { action: 'chatgpt_official_status', app, session_id: session.id } });
      if (r?.session) { error = ''; save(r.session); } else status.textContent = r?.msg || '暂时无法读取登录进度，正在重试。';
    } catch { status.textContent = '暂时无法读取登录进度，正在重试。'; }
    schedule();
  }
  async function confirmCancel() {
    if (starting) { status.textContent = '正在处理登录请求，请稍候再取消。'; return false; }
    return typeof askConfirm !== 'function' || await askConfirm(session?.phase === 'failed' ? '清理临时配置并结束这次登录，以便重新开始？' : '取消这次官方登录并清理临时配置？', { okText: session?.phase === 'failed' ? '清理并重新开始' : '取消本次登录' });
  }
  draw(); schedule();
  return { node: box, active, async beforeClose() {
    if (!active()) return true;
    if (!await confirmCancel()) return false;
    closeAfterCancel = true;
    if (!await request('chatgpt_official_cancel')) { closeAfterCancel = false; return false; }
    if (active()) { status.textContent = '正在取消并清理临时配置，完成后可关闭。'; return false; }
    return true;
  }, dispose() { disposed = true; clearTimeout(timer); } };
}

function openChatgptAdd(app, options = {}) {
  const appId = app.id;
  const panelId = `cpt-add-method-${++chatgptDialogCounter}`;
  const fields = {}, inputNodes = [];
  let selected = 'client', busy = false, removed = false, official = null, oauthName = '', oauthTimer, oauthRunning = false;
  const overlay = el('div', { class: 'cpt-overlay' });
  const name = el('input', { placeholder: '留空自动命名', maxLength: 60, autocomplete: 'off', aria: { label: '账号名称（可选）' } });
  const history = el('select', { value: 'shared', aria: { label: '新账号的对话和项目' } },
    el('option', { value: 'shared' }, '共用本机对话和项目，登录独立'));
  const note = el('div', { class: 'hintline', role: 'status', aria: { live: 'polite' } });
  const body = el('div', { class: 'cpt-method', role: options.existingName ? 'group' : 'tabpanel', id: panelId });
  const tabs = el('div', { class: 'ctlrow cpt-tabs', role: 'tablist', aria: { label: '添加账号方式' } });
  const tabButtons = [];
  function setBusy(value) {
    busy = value;
    const pending = selected === 'client' && chatgptOfficialSessions.get(`${appId}:${options.existingName || 'new'}`);
    name.disabled = value || !!pending; history.disabled = value || !!pending;
    tabButtons.forEach((button) => { button.disabled = value; });
  }
  function field(key, attrs = {}) {
    const { multiline, ...rest } = attrs;
    const node = el(multiline ? 'textarea' : 'input', { value: fields[key] || '', autocomplete: 'off', ...rest });
    inputNodes.push(node);
    node.addEventListener('input', () => { fields[key] = node.value; }); return node;
  }
  function remove() {
    removed = true; official?.dispose(); clearTimeout(oauthTimer);
    inputNodes.forEach((node) => { node.value = ''; });
    Object.keys(fields).forEach((key) => { fields[key] = ''; }); overlay.remove();
  }
  async function completed(accountName) {
    if (removed) return;
    remove();
    try {
      S.data = await invoke('get_state');
      S.cur = appId; S.view = 'agents';
      if (typeof closeDetailNow === 'function') closeDetailNow(); else S.detail = null;
      S.chatgptReveal = { app: appId, name: accountName, until: Date.now() + 6000 };
      render();
      const reveal = () => {
        const cards = document.querySelectorAll?.('.card[data-app="codex-desktop"]') || [];
        const card = [...cards].find((node) => node.dataset.name === accountName);
        card?.scrollIntoView?.({ block: 'nearest', behavior: 'smooth' });
        card?.focus?.({ preventScroll: true });
      };
      if (typeof requestAnimationFrame === 'function') requestAnimationFrame(reveal); else reveal();
      toast(`已添加${accountName ? '「' + accountName + '」' : '账号'}`, true);
      if (accountName) chatgptRefreshQuota(appId, accountName);
    }
    catch (e) { toast(`账号已保存，刷新列表失败：${String(e)}`, false); }
  }
  async function close() {
    if (official && !await official.beforeClose()) return;
    if (oauthRunning) {
      if (typeof askConfirm === 'function' && !await askConfirm('关闭会取消本次 OAuth 授权，已登记的账号会保留。', { okText: '取消授权并关闭' })) return;
      try {
        const r = await invoke('action', { payload: { action: 'chatgpt_cancel', app: appId, name: oauthName } });
        if (!r?.ok) { note.textContent = r?.msg || '取消未完成，请稍后重试。'; return; }
      } catch (e) { note.textContent = String(e); return; }
    } else if (busy) { note.textContent = '正在保存账号，请稍候。'; return; }
    remove();
  }
  async function add(action, extra) {
    if (busy) return null;
    setBusy(true); note.textContent = '正在处理…';
    try {
      const r = await invoke('action', { payload: { action, app: appId, new_name: name.value.trim(), history_mode: history.value, ...extra } });
      if (!r?.ok) { note.textContent = r?.msg || '账号未能添加，请检查后重试。'; return null; }
      return r;
    } catch (e) { note.textContent = String(e); return null; }
    finally { setBusy(false); }
  }
  async function pollOAuth() {
    if (removed || !oauthName) return;
    try {
      const r = await invoke('action', { payload: { action: 'chatgpt_status', app: appId, name: oauthName } });
      if (r?.ok) {
        const value = r.status || {};
        oauthRunning = !!value.login?.running;
        note.textContent = value.login?.message || (value.has_auth ? '授权已完成。' : '等待完成官方授权。');
        const url = fields.oauthUrlNode; if (url) url.value = value.login?.url || '';
        if (value.has_auth && !oauthRunning) { await completed(oauthName); return; }
        if (!oauthRunning) setBusy(false);
      }
    } catch { note.textContent = '暂时无法读取授权状态，正在重试。'; }
    if (!removed && oauthRunning) oauthTimer = setTimeout(pollOAuth, 1500);
  }
  function show() {
    official?.dispose(); official = null; body.replaceChildren(); note.textContent = '';
    setBusy(false);
    if (!options.existingName) body.setAttribute('aria-labelledby', `${panelId}-${selected}`);
    tabButtons.forEach((button) => { const active = button.cptTab === selected; button.setAttribute('aria-selected', String(active)); button.className = `btn sm${active ? ' primary' : ''}`; });
    if (selected === 'client') {
      official = chatgptOfficialPanel(appId, { name: options.existingName, newName: () => name.value.trim(), historyMode: () => history.value,
        onSuccess: completed, onBusy: setBusy, onCloseReady: remove });
      body.appendChild(official.node);
    } else if (selected === 'oauth') {
      const url = el('input', { readOnly: true, placeholder: '开始授权后显示官方链接', aria: { label: '官方授权链接' } }); fields.oauthUrlNode = url; inputNodes.push(url);
      const callback = field('callback', { placeholder: '需要手动提交时，粘贴完整回调地址', spellcheck: false, aria: { label: '授权回调地址' } });
      async function startOAuth(method) {
        let r;
        if (oauthName) {
          if (busy) return; setBusy(true);
          try { r = await invoke('action', { payload: { action: 'chatgpt_login', app: appId, name: oauthName, method } }); }
          catch (e) { note.textContent = String(e); setBusy(false); return; }
          if (!r?.ok) { note.textContent = r?.msg || '授权未能开始。'; setBusy(false); return; }
        } else { r = await add('chatgpt_add_oauth', { method }); if (!r) return; oauthName = r.name; }
        oauthRunning = true; setBusy(true); note.textContent = r.msg || '请在官方授权页面完成登录。';
        await pollOAuth();
      }
      body.append(el('div', { class: 'ctlrow' }, btn('浏览器授权', 'primary sm', () => startOAuth('browser')), btn('设备码登录', 'sm', () => startOAuth('device'))), url,
        el('div', { class: 'ctlrow' }, btn('打开授权页', 'sm', async () => { if (oauthName) await run('chatgpt_open_authorization', { app: appId, name: oauthName }); }),
          btn('无痕窗口授权', 'sm', async () => { if (oauthName) await run('chatgpt_open_authorization', { app: appId, name: oauthName, private: true }); })), callback,
        btn('提交本次回调', 'sm', async () => {
          if (!oauthName) { note.textContent = '请先开始授权。'; return; }
          const r = await run('chatgpt_callback', { app: appId, name: oauthName, url: callback.value });
          if (r?.ok) { fields.callback = ''; callback.value = ''; await pollOAuth(); }
        }));
    } else if (selected === 'token') {
      const text = field('token', { multiline: true, rows: 7, spellcheck: false, placeholder: '粘贴 auth.json、Token JSON、access token 或 refresh token', aria: { label: '待导入凭据' } });
      const base = field('tokenBase', { type: 'url', placeholder: '可选 Base URL', aria: { label: 'Token Base URL' } });
      const file = el('input', { type: 'file', accept: '.json,.txt,application/json,text/plain', aria: { label: '选择凭据文件' } });
      file.addEventListener('change', async () => {
        const chosen = file.files?.[0]; if (!chosen) return;
        if (chosen.size > 8 * 1024 * 1024) { note.textContent = '凭据文件不能超过8MB。'; return; }
        try { fields.token = await chosen.text(); text.value = fields.token; } catch { note.textContent = '文件读取失败。'; }
      });
      body.append(text, file, base, btn('导入并添加账号', 'primary sm', async () => { const r = await add('chatgpt_add_import', { text: text.value, base_url: base.value }); if (r) await completed(r.name); }));
    } else if (selected === 'apikey') {
      const key = field('key', { type: 'password', placeholder: 'API Key', aria: { label: 'API Key' } });
      const base = field('base', { type: 'url', placeholder: 'Base URL，留空使用官方接口', aria: { label: 'API Base URL' } });
      body.append(key, base, btn('保存并添加账号', 'primary sm', async () => { const r = await add('chatgpt_add_apikey', { key: key.value, base_url: base.value }); if (r) await completed(r.name); }));
    } else {
      const source = field('source', { placeholder: '留空读取本机默认 .codex；也可填写 auth.json 或其目录', spellcheck: false, aria: { label: '本机凭据来源' } });
      body.append(source,
        btn('从本机导入并添加', 'primary sm', async () => { const r = await add('chatgpt_add_local', { source: source.value }); if (r) await completed(r.name); }));
    }
  }
  if (!options.existingName) for (const [id, title] of [['client', '官方登录'], ['oauth', 'OAuth'], ['token', 'Token / JSON'], ['apikey', 'API Key'], ['local', '导入']]) {
    const button = btn(title, 'sm', () => { if (!busy) { selected = id; show(); } }); button.cptTab = id;
    button.id = `${panelId}-${id}`; button.setAttribute('role', 'tab'); button.setAttribute('aria-controls', panelId); tabs.appendChild(button); tabButtons.push(button);
  }
  const dialog = el('div', { class: 'cpt-add', role: 'dialog', aria: { modal: 'true', label: options.existingName ? '更新官方登录' : '添加 ChatGPT 账号' } },
    el('h2', {}, options.existingName ? `更新「${options.existingName}」的官方登录` : '添加 ChatGPT 账号'));
  if (!options.existingName) dialog.append(el('label', {}, '账号名称（可选）', name),
    el('label', {}, el('span', { class: 'cpt-field-label' }, '新账号的对话和项目',
      qhelp('所有账号共用一份本机对话和项目，聊天正文不重复保存。每个账号的登录资料独立；已打开的列表可能需要刷新。')), history), tabs);
  dialog.append(body, note, el('div', { class: 'ctlrow' }, btn('关闭', '', close)));
  overlay.appendChild(dialog); document.body.appendChild(overlay); show(); if (!options.existingName) name.focus();
  overlay.addEventListener('keydown', (e) => { if (e.key === 'Escape') { e.stopPropagation(); close(); } });
  overlay.addEventListener('click', (e) => { if (e.target === overlay) close(); });
  return overlay;
}

const CHATGPT_DETAIL_SECTIONS = [
  ['info', '概览'], ['quota', '额度与用量'], ['run', '运行管理'],
  ['history', '对话与项目'], ['data', '文件位置'], ['auth', '登录管理'], ['danger', '移除账号']
];
const chatgptQuotaRequests = new Map();
const chatgptQuotaAttempts = new Set();
function chatgptQuotaValue(instance) {
  return instance.chatgpt_quota || instance.chatgpt_auth?.quota || {};
}
function chatgptNumber(value) {
  return value !== null && value !== undefined && value !== '' && Number.isFinite(Number(value)) ? Number(value) : null;
}
function chatgptQuotaHasReadings(quota) {
  return quota.balance?.unlimited === true || (quota.windows || []).some((w) => chatgptNumber(w.remaining_percent) !== null || chatgptNumber(w.used_percent) !== null)
    || ['remaining', 'total', 'used'].some((key) => chatgptNumber(quota.balance?.[key]) !== null);
}
function chatgptTimestamp(value) {
  if (!value) return '';
  const numeric = Number(value);
  const date = Number.isFinite(numeric) ? new Date(numeric < 1e12 ? numeric * 1000 : numeric) : new Date(value);
  return Number.isNaN(date.getTime()) ? '' : date.toLocaleString('zh-CN', { month: 'numeric', day: 'numeric', hour: '2-digit', minute: '2-digit' });
}
async function chatgptRefreshQuota(app, name) {
  const key = `${app}:${name}`;
  if (chatgptQuotaRequests.has(key)) return chatgptQuotaRequests.get(key);
  chatgptQuotaAttempts.add(key);
  const task = (async () => {
    const instance = S.data?.instances?.find((i) => i.app === app && i.name === name);
    const save = (quota) => {
      const current = S.data?.instances?.find((i) => i.app === app && i.name === name) || instance;
      if (current) current.chatgpt_quota = quota;
    };
    try {
      const result = await invoke('action', { payload: { action: 'chatgpt_quota_refresh', app, name } });
      save(result.quota || {
        ...chatgptQuotaValue(instance || {}), state: chatgptQuotaHasReadings(chatgptQuotaValue(instance || {})) ? 'cached' : 'error',
        message: result.msg || '未能读取额度，请重试。'
      });
      return result;
    } catch (error) {
      save({ ...chatgptQuotaValue(instance || {}),
        state: chatgptQuotaHasReadings(chatgptQuotaValue(instance || {})) ? 'cached' : 'error', message: String(error) });
      return { ok: false, msg: String(error) };
    } finally {
      chatgptQuotaRequests.delete(key);
      if (S.data && typeof render === 'function') render();
      if (S.detail?.app === app && S.detail.name === name && ['info', 'quota'].includes(S.detail.sec) && typeof renderDetail === 'function') renderDetail();
    }
  })();
  chatgptQuotaRequests.set(key, task);
  return task;
}
function chatgptQuotaPanel(instance, options = {}) {
  const quota = chatgptQuotaValue(instance), auth = instance.chatgpt_auth || {};
  const key = `${instance.app}:${instance.name}`;
  const busy = chatgptQuotaRequests.has(key);
  const hasReadings = chatgptQuotaHasReadings(quota);
  const state = busy ? 'loading' : quota.state || 'unread';
  const mode = quota.mode || auth.mode;
  const panel = el('div', { class: `cpt-quota ${options.compact ? 'is-compact' : ''} ${state}`, role: 'group', aria: { label: '账号额度' } });
  const refresh = btn(busy ? '读取中…' : '刷新额度', 'sm', async (event) => {
    event?.stopPropagation?.(); refresh.disabled = true; refresh.textContent = '读取中…';
    await chatgptRefreshQuota(instance.app, instance.name);
  });
  refresh.disabled = busy;
  panel.appendChild(el('div', { class: 'cpt-quota-head' },
    el('strong', {}, mode === 'apikey' ? 'API 余额' : '账号额度'), refresh));
  if (state === 'cached' && hasReadings) panel.appendChild(el('p', { class: 'cpt-quota-message warning' }, quota.message || '本次读取未成功，显示上次成功读取的结果。'));
  for (const window of quota.windows || []) {
    let remaining = chatgptNumber(window.remaining_percent), used = chatgptNumber(window.used_percent);
    if (remaining === null && used !== null) remaining = 100 - used;
    if (remaining === null) continue;
    remaining = Math.max(0, Math.min(100, remaining));
    const reset = chatgptTimestamp(window.reset_at);
    const label = window.label || (window.id === 'weekly' ? '每周限额' : window.id === '5h' ? '5 小时限额' : '使用限额');
    panel.appendChild(el('div', { class: 'cpt-quota-window' },
      el('div', { class: 'cpt-quota-line' }, el('span', {}, label), el('b', {}, `剩余 ${remaining.toFixed(remaining % 1 ? 1 : 0)}%`)),
      el('progress', { max: 100, value: remaining, aria: { label: `${label}剩余比例` } }),
      reset ? el('small', {}, `${reset} 重置`) : null));
  }
  if (quota.balance) {
    const balance = quota.balance;
    const amount = (value) => Number(value).toLocaleString('zh-CN', { maximumFractionDigits: 4 });
    const unit = balance.currency || balance.unit || '';
    const remaining = chatgptNumber(balance.remaining);
    if (balance.unlimited) panel.appendChild(el('div', { class: 'cpt-balance' }, el('span', {}, '令牌限额'), el('strong', {}, '不限额')));
    else if (remaining !== null) panel.appendChild(el('div', { class: 'cpt-balance' }, el('span', {}, '可用余额'), el('strong', {}, `${amount(remaining)}${unit ? ' ' + unit : ''}`)));
    const detail = [['total', '总额度'], ['used', '已用']].filter(([field]) => chatgptNumber(balance[field]) !== null);
    if (detail.length) panel.appendChild(el('div', { class: 'cpt-balance-details' }, ...detail.map(([field, title]) => el('span', {}, `${title} ${amount(balance[field])}${unit ? ' ' + unit : ''}`))));
  }
  if (!hasReadings) {
    const message = state === 'loading' ? '正在读取这个账号的额度。'
      : quota.message || (state === 'unsupported' ? '该服务暂未提供可读取的余额接口。' : state === 'error' ? '本次额度读取失败，请重试。' : '尚未读取这个账号的额度。');
    panel.appendChild(el('p', { class: `cpt-quota-message ${state === 'error' ? 'error' : ''}` }, message));
  }
  const at = chatgptTimestamp(quota.fetched_at);
  if (hasReadings && at) panel.appendChild(el('small', { class: 'cpt-quota-time' }, `更新于 ${at}`));
  if (auth.has_auth && !chatgptQuotaAttempts.has(key) && !quota.fetched_at && !busy) {
    chatgptQuotaAttempts.add(key);
    setTimeout(() => chatgptRefreshQuota(instance.app, instance.name), 0);
  }
  return panel;
}
function chatgptInfoRow(label, value, note) {
  return el('div', { class: 'cpt-info-row' }, el('div', { class: 'cpt-info-label' }, label, note ? qhelp(note) : null),
    el('div', { class: 'cpt-info-value' }, typeof value === 'object' ? value : el('span', {}, value || '未提供')));
}
function chatgptDataPanel(app, name, instance) {
  const auth = instance.chatgpt_auth || {};
  function location(title, description, path, action) {
    const available = typeof path === 'string' && !!path.trim();
    const copy = btn('复制路径', 'sm', async () => {
      try { await navigator.clipboard.writeText(path); toast('已复制路径', true); }
      catch { toast('复制未完成，请手动选择路径复制。', false); }
    });
    const open = btn('打开文件夹', 'sm', () => run(action, { app, name }));
    copy.disabled = !available; open.disabled = !available;
    return el('article', { class: 'cpt-location' }, el('h3', {}, title, qhelp(description)),
      el('code', { class: 'cpt-path', aria: { label: title + '路径' } }, available ? path : '尚未记录这个目录的位置'),
      el('div', { class: 'cpt-actions' }, open, copy));
  }
  return secPanel('文件位置', el('div', { class: 'cpt-settings' },
    location('客户端资料', '界面设置和浏览器资料。', instance.data_dir || auth.profile, 'open_dir'),
    location('对话与账号数据', '对话、项目索引、运行配置和登录信息。移除账号登记后仍保留。', auth.home, 'chatgpt_open_home')));
}
function chatgptSettingsPanel(app, name, instance, detail) {
  const auth = instance.chatgpt_auth || {}, snapshot = auth.history_snapshot || {};
  const active = instance.running || instance.resident;
  const state = instance.running ? '运行中' : instance.resident ? '后台运行' : '未运行';
  const open = () => btn(active ? '打开窗口' : '启动', 'primary sm', () => run('chatgpt_show_window', { app, name }));
  const stop = () => btn(instance.resident ? '结束后台进程' : '结束运行', 'danger sm', () => run('kill', { app, name }));
  const actions = () => el('div', { class: 'cpt-actions' },
    open(), active ? stop() : null);
  const mode = auth.mode === 'apikey' ? 'API Key' : 'ChatGPT 账号';
  if (detail.sec === 'quota') return secPanel('额度与用量', chatgptQuotaPanel(instance));
  if (detail.sec === 'data') return chatgptDataPanel(app, name, instance);
  if (detail.sec === 'auth') return chatgptAuthPanel(app, name, instance);
  if (detail.sec === 'run') return secPanel('运行管理', el('div', { class: 'cpt-settings cpt-run' },
    el('div', { class: 'cpt-run-summary' },
      el('div', { class: 'cpt-run-state' }, el('span', { class: 'cpt-field-label' }, '运行状态'), el('strong', {}, state)), open()),
    active ? el('div', { class: 'cpt-run-stop' },
      el('div', { class: 'cpt-run-stop-label' }, el('strong', {}, '结束运行'), qhelp('仅结束此账号的客户端。请先保存未完成内容。')), stop()) : null));
  if (detail.sec === 'history') {
    const repair = auth.project_repair || {};
    const status = el('p', { class: 'cpt-note', role: 'status', aria: { live: 'polite' } }, repair.pending
      ? '已排队，将在下次启动时修正。' : repair.state === 'applied' ? '项目列表已修正。' : repair.state === 'current' ? '项目列表正常。' : repair.message || repair.last_result?.message || '');
    const fix = btn(repair.pending ? '已安排修正' : '修正项目列表', 'sm', async () => {
      fix.disabled = true;
      try {
        const result = await invoke('action', { payload: { action: 'chatgpt_project_repair', app, name } });
        if (result.project_repair) auth.project_repair = result.project_repair;
        status.textContent = result.ok && result.project_repair?.pending ? '已排队，将在下次启动时修正。'
          : result.msg || result.project_repair?.message || (result.ok ? '项目列表已检查。' : '未完成修正，请重试。');
        fix.textContent = result.project_repair?.pending ? '已安排修正' : '重新检查项目列表';
        fix.disabled = !!result.project_repair?.pending;
      } catch (error) { status.textContent = String(error); fix.disabled = false; }
    });
    fix.disabled = !!repair.pending;
    const count = repair.repairable_count ?? repair.detected_count;
    return secPanel('对话与项目', el('div', { class: 'cpt-settings' },
      chatgptInfoRow('对话记录', snapshot.threads != null ? `${snapshot.threads} 条对话` : '由客户端管理'),
      chatgptInfoRow('项目列表', snapshot.visible_projects != null ? `${snapshot.visible_projects} 个项目` : '由客户端管理', '已归档、隐藏或移除的项目保持原状态。'),
      chatgptInfoRow('对话存储', auth.history_storage?.mode === 'shared' ? '共用一份本机历史' : '等待接入共享历史',
        auth.history_storage?.mode === 'shared' ? '聊天正文和历史数据库统一保存，登录资料独立。' : '关闭此账号窗口后，下次启动会接入统一历史。'),
      snapshot.missing_archived_rollouts ? chatgptInfoRow('归档记录', `${snapshot.missing_archived_rollouts} 条原文件缺失`, '保留原有索引，未重建空白对话。') : null,
      count > 0 ? chatgptInfoRow('项目检查', `${count} 个项目可修正`) : null,
      el('div', { class: 'cpt-actions' }, fix, qhelp('仅修正异常项目登记，保留项目文件和对话。运行中将在下次启动时应用。')), status));
  }
  if (detail.sec === 'danger') return secPanel('移除账号', el('div', { class: 'cpt-settings' },
    el('p', {}, '仅移除账号登记，保留对话、项目和登录数据。'),
    el('div', { class: 'cpt-actions' }, btn('移除这张卡片', 'danger sm', async () => { if (await del(app, name)) closeDetailNow(); }))));
  return secPanel('账号概览', el('div', { class: 'cpt-settings' },
    el('div', { class: 'cpt-account-head' }, el('strong', { class: name.includes('@') ? 'account-email' : 'account-title' }, name), el('span', { class: `cpt-status ${auth.has_auth ? 'is-ready' : ''}` }, auth.has_auth ? '登录信息已保存' : '尚未登录')),
    chatgptInfoRow('登录方式', mode),
    chatgptInfoRow(auth.mode === 'apikey' ? 'API 服务' : '登录账号', auth.mode === 'apikey' ? auth.base_url || '官方 API' : auth.account || '尚未读取到邮箱'),
    chatgptInfoRow('运行状态', state),
    el('div', { class: 'cpt-actions' }, ...actions().children,
      btn('修改账号名称', 'sm', () => renameAccount(app, name))),
    chatgptQuotaPanel(instance, { compact: true })));
}
function renderChatgptDetail(detail, instance) {
  $('im-save').hidden = true;
  if (!CHATGPT_DETAIL_SECTIONS.some(([id]) => id === detail.sec)) detail.sec = 'info';
  if (detail.sec !== 'auth') clearTimeout(detail.chatgptTimer);
  const nav = $('im-nav'); clear(nav);
  for (const [id, title] of CHATGPT_DETAIL_SECTIONS) {
    if (id === 'danger') nav.appendChild(el('li', { class: 'sep', aria: { hidden: 'true' } }));
    const button = btn(title, id === 'danger' ? 'danger' : '', () => {
      detail.sec = id; detail.scroll = 0; renderDetail();
    });
    button.setAttribute('aria-current', String(id === detail.sec));
    nav.appendChild(el('li', {}, button));
  }
  const body = $('im-body'); clear(body);
  body.appendChild(chatgptSettingsPanel(detail.app, detail.name, instance, detail));
  if (typeof restoreScroll === 'function') restoreScroll(body, detail.scroll || 0); else body.scrollTop = detail.scroll || 0;
}

function chatgptAuthPanel(app, name, instance) {
  const detail = S.detail;
  const fields = detail.chatgptFields || (detail.chatgptFields = {});
  const initial = instance.chatgpt_auth || {};
  const native = !!initial.native || !!instance.in_place;
  if (initial.has_auth && !detail.chatgptEditLogin) {
    clearTimeout(detail.chatgptTimer);
    const startEdit = () => {
      detail.chatgptEditLogin = true;
      detail.chatgptTab = initial.mode === 'apikey' ? 'apikey' : 'client';
      renderDetail();
    };
    return secPanel('登录管理', el('div', { class: 'cpt-settings' },
      el('div', { class: 'cpt-account-head' }, el('strong', {}, '当前登录'), el('span', { class: 'cpt-status is-ready' }, '登录信息已保存')),
      chatgptInfoRow('登录方式', initial.mode === 'apikey' ? 'API Key' : 'ChatGPT 账号'),
      chatgptInfoRow(initial.mode === 'apikey' ? 'API 服务' : '登录账号', initial.mode === 'apikey' ? initial.base_url || '官方 API' : initial.account || '未提供邮箱'),
      el('div', { class: 'cpt-actions' }, native
        ? btn('添加独立账号', 'sm', () => openChatgptAdd(S.data.apps.find((a) => a.id === app) || { id: app }))
        : btn(initial.mode === 'apikey' ? '更换 API Key' : '重新授权 / 更换登录', 'sm', startEdit))));
  }
  const status = el('div', { class: 'hintline', role: 'status', aria: { live: 'polite' } });
  const progress = el('pre', { class: 'cpt-progress' });
  const url = el('input', { readOnly: true, placeholder: '开始授权后显示官方链接', aria: { label: '官方授权链接' } });
  function field(key, attrs = {}) {
    const node = el(attrs.multiline ? 'textarea' : 'input', { value: fields[key] || '', autocomplete: 'off', ...attrs });
    node.addEventListener('input', () => { fields[key] = node.value; }); return node;
  }
  function update(value) {
    const label = value.has_auth ? `${value.mode === 'apikey' ? 'API Key' : 'ChatGPT 账号'}凭据已保存${value.account ? ` · ${value.account}` : ''}`
      : native ? '本机登录' : '未读取到登录信息';
    status.textContent = value.path_error || (value.login?.running ? value.login.message : label);
    progress.textContent = value.login?.output || value.login?.message || ''; progress.hidden = !progress.textContent;
    url.value = value.login?.url || '';
  }
  async function refresh() {
    if (S.detail !== detail || detail.sec !== 'auth') return;
    try { const r = await invoke('action', { payload: { action: 'chatgpt_status', app, name } }); if (r?.ok) update(r.status); }
    catch (e) { status.textContent = `读取登录状态失败：${String(e)}`; }
    if (S.detail === detail && detail.sec === 'auth') detail.chatgptTimer = setTimeout(refresh, 1800);
  }
  async function act(action, extra = {}) {
    const r = await run(action, { app, name, ...extra });
    if (r?.status) update(r.status);
    if (r?.ok && ['chatgpt_import', 'chatgpt_apikey', 'chatgpt_local_import'].includes(action)) {
      detail.chatgptEditLogin = false; renderDetail();
    }
    return r;
  }
  update(initial); clearTimeout(detail.chatgptTimer); detail.chatgptTimer = setTimeout(refresh, 200);
  const tab = detail.chatgptTab || (initial.mode === 'apikey' ? 'apikey' : 'client');
  const tabs = el('div', { class: 'ctlrow cpt-tabs', role: 'tablist', aria: { label: '登录方式' } });
  for (const [id, title] of [['client', '官方登录'], ['oauth', 'OAuth'], ['token', 'Token / JSON'], ['apikey', 'API Key'], ['local', '导入']]) {
    const b = btn(title, `sm${tab === id ? ' primary' : ''}`, () => { detail.chatgptTab = id; renderDetail(); });
    b.setAttribute('role', 'tab'); b.setAttribute('aria-selected', String(tab === id)); tabs.appendChild(b);
  }
  const body = el('div', { class: 'cpt-method' });
  if (tab === 'client') {
    body.append(btn(native ? '添加独立账号' : '开始官方登录', 'primary sm', () => openChatgptAdd(S.data.apps.find((a) => a.id === app) || { id: app }, native ? {} : { existingName: name })));
  } else if (native) {
    body.append(btn('新建独立账号', 'primary sm', () => openChatgptAdd(S.data.apps.find((a) => a.id === app))), qhelp('本机默认登录保持不变。'));
  } else if (tab === 'oauth') {
    const callback = field('callback', { placeholder: '需要手动提交时，粘贴完整的本机回调地址', spellcheck: false, aria: { label: '授权回调地址' } });
    body.append(el('div', { class: 'ctlrow' }, btn('浏览器授权', 'primary sm', () => act('chatgpt_login', { method: 'browser' })),
        btn('设备码登录', 'sm', () => act('chatgpt_login', { method: 'device' })), btn('取消本次授权', 'sm', () => act('chatgpt_cancel'))), url,
      el('div', { class: 'ctlrow' }, btn('打开授权页', 'sm', () => act('chatgpt_open_authorization')),
        btn('无痕窗口授权', 'sm', () => act('chatgpt_open_authorization', { private: true })),
        btn('复制授权链接', 'sm', async () => {
          if (!url.value) { toast('开始授权后才会生成链接', false); return; }
          try { await navigator.clipboard.writeText(url.value); toast('已复制授权链接', true); }
          catch { url.focus(); url.select(); toast('链接已选中，可按 Ctrl+C 复制', true); }
        })), callback, btn('提交本次回调', 'sm', async () => {
        const r = await act('chatgpt_callback', { url: callback.value }); if (r?.ok) { fields.callback = ''; callback.value = ''; }
      }));
  } else if (tab === 'token') {
    const text = field('importText', { multiline: true, rows: 7, spellcheck: false, placeholder: '粘贴 auth.json、Token JSON、access token 或 refresh token', aria: { label: '待导入凭据' } });
    const file = el('input', { type: 'file', accept: '.json,.txt,application/json,text/plain', aria: { label: '选择凭据文件' } });
    file.addEventListener('change', async () => {
      const selected = file.files?.[0]; if (!selected) return;
      if (selected.size > 8 * 1024 * 1024) { toast('凭据文件不能超过8MB', false); return; }
      try { fields.importText = await selected.text(); text.value = fields.importText; } catch { toast('文件读取失败', false); }
    });
    const prefix = field('batchPrefix', { placeholder: '批量账号名前缀，例如 工作账号', maxLength: 48, aria: { label: '批量账号名前缀' } });
    const results = el('pre', { class: 'cpt-progress', hidden: !fields.batchResults, role: 'status' }, fields.batchResults || '');
    body.append(el('label', {}, el('span', { class: 'cpt-field-label' }, '登录凭据', qhelp('支持 auth.json、Token JSON、ChatGPT session JSON 或单个令牌。')), text), file,
      btn('导入这份凭据', 'primary sm', async () => { const r = await act('chatgpt_import', { text: text.value }); if (r?.ok) { fields.importText = ''; text.value = ''; file.value = ''; renderDetail(); } }),
      el('label', {}, el('span', { class: 'cpt-field-label' }, '批量导入', qhelp('使用 JSON 数组或每行一个令牌。新账号按“前缀-01”命名，现有账号保留。')), prefix),
      btn('批量创建独立账号', 'sm', async () => {
        if (!prefix.value.trim()) { prefix.focus(); toast('请填写批量账号名前缀', false); return; }
        const r = await act('chatgpt_import_many', { text: text.value, prefix: prefix.value });
        if (r?.rows) { results.hidden = false; fields.batchResults = r.rows.map(v => `${v.name}：${v.ok ? '成功' : '未完成'}，${v.msg}`).join('\n'); results.textContent = fields.batchResults; }
        if (r?.all_ok) { fields.importText = ''; text.value = ''; file.value = ''; }
        renderDetail();
      }), results);
  } else if (tab === 'apikey') {
    const key = field('apiKey', { type: 'password', placeholder: 'API Key', aria: { label: 'API Key' } });
    const base = field('apiBase', { type: 'url', placeholder: '可选 Base URL，默认使用官方接口', aria: { label: 'API Base URL' } });
    body.append(key, base,
      btn('保存 API Key', 'primary sm', async () => { const r = await act('chatgpt_apikey', { key: key.value, base_url: base.value }); if (r?.ok) { fields.apiKey = ''; key.value = ''; renderDetail(); } }));
  } else {
    const source = field('localSource', { placeholder: '留空读取本机默认 .codex；也可填写 auth.json 或其目录', spellcheck: false, aria: { label: '本机凭据来源' } });
    body.append(source,
      btn('从本机导入', 'primary sm', () => act('chatgpt_local_import', { source: source.value })));
  }
  return secPanel(initial.has_auth ? '更新登录信息' : '登录这个账号',
    initial.has_auth ? el('div', { class: 'cpt-actions' }, btn('返回当前登录', 'sm', () => { detail.chatgptEditLogin = false; detail.chatgptFields = {}; renderDetail(); })) : null,
    status, tabs, body, progress);
}
