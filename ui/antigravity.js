/* Antigravity 账号：登录态整机一份（Windows 凭据管理器），切换只切凭据 + 重启客户端。 */
const ANTIGRAVITY_SECTIONS = [['info', '概览'], ['quota', '额度与用量'], ['auth', '登录管理'], ['danger', '移除账号']];
const ANTIGRAVITY_BUSY = new Set();

async function antigravityAction(action, name = '', extra = {}) {
  const key = ['antigravity_switch', 'antigravity_capture', 'kill'].includes(action) ? 'login-change' : `${action}:${name}`;
  if (ANTIGRAVITY_BUSY.has(key)) return { ok: false, busy: true };
  ANTIGRAVITY_BUSY.add(key);
  try {
    const payload = { action, app: 'antigravity', name, ...extra };
    let result = await invoke('action', { payload });
    if (result?.need_force) {
      if (!await askConfirm('此操作需要关闭 Antigravity。请先保存未完成的内容。',
        { title: '切换 Antigravity 账号', okText: '关闭并切换' })) {
        return { ok: false, cancelled: true };
      }
      result = await invoke('action', { payload: { ...payload, force: true } });
    }
    toast(result?.msg || (result?.ok ? '操作完成' : '操作失败'), !!result?.ok);
    if (result?.ok) await load();
    return result || { ok: false };
  } catch (error) {
    const msg = String(error?.message || error);
    toast(msg, false);
    return { ok: false, msg };
  } finally { ANTIGRAVITY_BUSY.delete(key); }
}

function antigravityOpenButton(instance) {
  const auth = instance.antigravity_auth || {};
  if (auth.current) return btn('打开窗口', 'primary sm', () => antigravityAction('launch', instance.name));
  if (auth.ready) return btn('切换并打开', 'primary sm', () => antigravityAction('antigravity_switch', instance.name));
  return btn('录入登录', 'primary sm', () => openDetail('antigravity', instance.name, 'auth'));
}

function antigravityDetailPanel(instance, section) {
  const name = instance.name, auth = instance.antigravity_auth || {};
  const current = !!(auth.current || instance.logged_in);
  const info = (label, value, note) => chatgptInfoRow(label, value, note);
  const loginState = auth.auth_status === 'locked' ? '保管箱未解锁' : auth.ready ? '凭据已保存' : '需要登录';
  if (section === 'quota') return secPanel('额度与用量',
    el('div', { class: 'cpt-actions' }, btn('刷新额度', 'sm', () => refreshQuota('antigravity', name))),
    quotaPanel(instance));
  if (section === 'auth') return secPanel('登录管理', el('div', { class: 'cpt-settings' },
    info('登录账号', auth.account || name), info('凭据状态', loginState),
    el('div', { class: 'cpt-actions' },
      current || auth.ready ? antigravityOpenButton(instance) : null,
      btn('重新授权 / 导入', 'sm', () => openAntigravityAdd({ existingName: name })),
      current && auth.ready ? btn('移除凭据', 'danger sm', () => antigravityAction('antigravity_remove', name)) : null)));
  if (section === 'danger') return secPanel('移除账号', el('div', { class: 'cpt-settings' },
    el('p', {}, '仅移除卡片。登录凭据与本机数据保留。'),
    el('div', { class: 'cpt-actions' }, btn('移除账号', 'danger sm', async () => {
      if (await del('antigravity', name)) closeDetailNow();
    }))));
  return secPanel('账号概览', el('div', { class: 'cpt-settings' },
    el('div', { class: 'cpt-account-head' }, el('strong', {}, name),
      el('span', { class: `cpt-status${auth.ready ? ' is-ready' : ''}` }, current ? '当前账号' : loginState)),
    info('登录账号', auth.account || name),
    info('额度', 'Google AI Pro / Free 按套餐'),
    el('div', { class: 'cpt-actions' }, antigravityOpenButton(instance), btn('修改名称', 'sm', () => renameAccount('antigravity', name))),
    quotaPanel(instance)));
}

function renderAntigravityDetail(detail, instance) {
  $('im-save').hidden = true;
  if (!ANTIGRAVITY_SECTIONS.some(([id]) => id === detail.sec)) detail.sec = 'info';
  const nav = $('im-nav'); clear(nav);
  for (const [id, label] of ANTIGRAVITY_SECTIONS) {
    if (id === 'danger') nav.appendChild(el('li', { class: 'sep' }));
    const button = btn(label, id === 'danger' ? 'danger' : '', () => { detail.sec = id; detail.scroll = 0; renderDetail(); });
    button.setAttribute('aria-current', String(id === detail.sec));
    nav.appendChild(el('li', {}, button));
  }
  const body = $('im-body'); clear(body);
  body.appendChild(antigravityDetailPanel(instance, detail.sec));
  restoreScroll(body, detail.scroll || 0);
}

/* 三选项卡「添加账号」：OAuth 授权 / refresh_token 导入 / 本机导入。 */
function openAntigravityAdd(options = {}) {
  const overlay = el('div', { class: 'cpt-overlay' });
  const status = el('div', { class: 'hintline', role: 'status', aria: { live: 'polite' } });
  const name = el('input', { value: options.existingName || '', placeholder: '账号名称', aria: { label: 'Antigravity 账号名称' } });
  let removed = false;
  const close = () => { removed = true; overlay.remove(); };

  const done = (ok, msg) => {
    status.textContent = msg || '';
    if (ok) { close(); S.cur = 'antigravity'; if (options.existingName && S.detail) S.detail.sec = 'info'; render(); }
  };

  /* Tab 1：OAuth 授权 */
  const oauthTab = el('div', { class: 'cpt-settings' },
    el('p', {}, '跳到浏览器用 Google 账号授权，授权成功后自动登记并写入登录凭据。'),
    el('div', { class: 'cpt-actions' }, btn('开始 OAuth 授权', 'primary', async () => {
      if (!name.value.trim()) { status.textContent = '请先填写账号名称。'; name.focus(); return; }
      status.textContent = '正在打开浏览器授权…';
      const result = await invoke('action', { payload: { action: 'antigravity_oauth_start', app: 'antigravity', name: name.value.trim() } });
      if (!result?.ok) { status.textContent = result?.msg || '无法启动授权'; return; }
      status.textContent = '请在浏览器中完成 Google 授权，此页会自动更新…';
      const deadline = Date.now() + 10 * 60 * 1000;
      const poll = async () => {
        const st = await invoke('action', { payload: { action: 'antigravity_oauth_status', app: 'antigravity' } });
        if (removed) return;
        if (st?.status === 'success') { done(true, `已登记 ${st.detail}`); }
        else if (st?.status === 'error') { status.textContent = st.detail || '授权失败'; }
        else if (Date.now() < deadline) setTimeout(poll, 1500);
      };
      poll();
    })));

  /* Tab 2：refresh_token 导入 */
  const tokenInput = el('textarea', { rows: 3, placeholder: '粘贴 refresh_token（1//…）', aria: { label: 'refresh_token' } });
  const emailInput = el('input', { placeholder: 'Google 邮箱（可选，凭据里能读到就不填）', aria: { label: '账号邮箱' } });
  const importTab = el('div', { class: 'cpt-settings' },
    el('p', {}, '直接粘贴 Google OAuth 的 refresh_token，也可整段粘贴账号 JSON。'),
    el('label', {}, 'refresh_token', tokenInput), el('label', {}, '邮箱（可选）', emailInput),
    el('div', { class: 'cpt-actions' }, btn('开始导入', 'primary', async () => {
      if (!name.value.trim()) { status.textContent = '请先填写账号名称。'; name.focus(); return; }
      const rt = tokenInput.value.trim();
      if (!rt) { status.textContent = '请粘贴 refresh_token。'; return; }
      const result = await antigravityAction('antigravity_import', name.value.trim(),
        { refresh_token: rt, email: emailInput.value.trim() });
      if (result.ok) done(true, result.msg); else if (result.msg) status.textContent = result.msg;
    })));

  /* Tab 3：本机导入 */
  const localTab = el('div', { class: 'cpt-settings' },
    el('p', {}, '读取本机当前登录的 Antigravity 账号（Windows 凭据管理器里的登录凭据）。'),
    el('div', { class: 'cpt-actions' }, btn('读取本机登录', 'primary', async () => {
      if (!name.value.trim()) { status.textContent = '请先填写账号名称。'; name.focus(); return; }
      const result = await antigravityAction('antigravity_capture', name.value.trim());
      if (result.ok) done(true, result.msg); else if (result.msg) status.textContent = result.msg;
    })));

  const tabs = [['oauth', 'OAuth 授权', oauthTab], ['token', 'Token / JSON', importTab], ['local', '本机导入', localTab]];
  let active = 'oauth';
  const tabBar = el('div', { class: 'cpt-tabs', role: 'tablist' });
  const bodies = el('div', {});
  const show = () => {
    clear(tabBar); clear(bodies);
    for (const [id, label] of tabs) {
      const t = btn(label, '', () => { active = id; show(); });
      t.setAttribute('aria-selected', String(id === active));
      t.setAttribute('role', 'tab');
      tabBar.appendChild(t);
    }
    bodies.appendChild(tabs.find(([id]) => id === active)[2]);
  };
  show();

  const dialog = el('div', { class: 'cpt-add', role: 'dialog', aria: { modal: 'true', label: '添加 Antigravity 账号' } },
    el('h2', {}, options.existingName ? 'Antigravity 登录管理' : '添加 Antigravity 账号',
      qhelp('Google 账号授权后凭据写入系统凭据库；多个账号共享本机对话，切换需重启客户端。')),
    el('label', {}, '账号名称', name), tabBar, bodies, status,
    el('div', { class: 'cpt-actions' }, btn('取消', '', close)));
  overlay.appendChild(dialog); document.body.appendChild(overlay);
  return overlay;
}
