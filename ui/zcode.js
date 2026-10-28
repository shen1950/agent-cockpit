/* ZCode keeps one local conversation store; each login has its own saved identity. */
const ZCODE_SECTIONS = [['info', '概览'], ['quota', '额度与用量'], ['run', '运行管理'],
  ['auth', '登录管理'], ['data', '文件位置'], ['danger', '移除账号']];
const ZCODE_BUSY = new Set();
function zcodeProvider(provider) { return provider === 'bigmodel' ? 'BigModel' : provider === 'zai' ? 'Z.ai' : '未识别'; }

async function zcodeAction(action, name = '', extra = {}) {
  const key = ['zcode_switch', 'zcode_logout', 'zcode_capture', 'kill'].includes(action) ? 'login-change' : `${action}:${name}`;
  if (ZCODE_BUSY.has(key)) return { ok: false, busy: true };
  ZCODE_BUSY.add(key);
  try {
    const payload = { action, app: 'zcode', name, ...extra };
    let result = await invoke('action', { payload });
    if (result?.need_force) {
      const logout = action === 'zcode_logout';
      const stop = action === 'kill';
      if (!await askConfirm(stop ? '将结束 ZCode。请先保存未完成的内容。' : '此操作需要重启 ZCode。请先保存未完成的内容。',
        { title: stop ? '结束 ZCode' : logout ? '退出 ZCode 登录' : '切换 ZCode 账号', okText: stop ? '结束' : logout ? '重启并退出' : '重启并切换' })) {
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
  } finally { ZCODE_BUSY.delete(key); }
}

function zcodeOpenButton(instance) {
  const auth = instance.zcode_auth || {};
  const current = auth.current || instance.logged_in;
  if (current) return btn('打开窗口', 'primary sm', () => zcodeAction('launch_app', instance.name));
  if (auth.has_auth) return btn('切换并打开', 'primary sm', () => zcodeAction('zcode_switch', instance.name));
  return btn('录入登录', 'primary sm', () => openDetail('zcode', instance.name, 'auth'));
}

function zcodeQuotaPanel(instance, options = {}) {
  const quota = instance.quota || {};
  if (instance.zcode_auth?.quota_state === 'no_plan' && !quota.live_error && !quota.stale) {
    return el('div', { class: 'cpt-quota' }, el('strong', {}, '暂无 ZCode 套餐'),
      quota.fetched_at ? el('small', { class: 'cpt-quota-time' }, `更新于 ${stampTime(quota.fetched_at)}`) : null);
  }
  return options.compact ? quotaBox(instance) : quotaPanel(instance);
}

function zcodeDetailPanel(instance, section) {
  const name = instance.name, auth = instance.zcode_auth || {};
  const current = !!(auth.current || instance.logged_in);
  const provider = zcodeProvider(auth.provider);
  const info = (label, value, note) => chatgptInfoRow(label, value, note);
  const loginState = auth.auth_status === 'locked' ? '保管箱未解锁' : auth.has_auth ? '凭据已保存' : '需要登录';
  if (section === 'quota') return secPanel('额度与用量',
    el('div', { class: 'cpt-actions' }, btn('刷新额度', 'sm', () => refreshQuota('zcode', name))), zcodeQuotaPanel(instance));
  if (section === 'run') return secPanel('运行管理', el('div', { class: 'cpt-settings cpt-run' },
    el('div', { class: 'cpt-run-summary' },
      el('div', { class: 'cpt-run-state' }, el('span', { class: 'cpt-field-label' }, '运行状态'),
        el('strong', {}, instance.running ? '运行中' : current ? '当前账号' : '未运行')), zcodeOpenButton(instance)),
    info('账号模式', '共享对话，独立凭据', '切换账号需重启 ZCode；本机对话和项目保留。'),
    current && (instance.running || instance.resident) ? el('div', { class: 'cpt-run-stop' },
      el('div', { class: 'cpt-run-stop-label' }, el('strong', {}, '结束运行'), qhelp('请先保存未完成内容。')),
      btn('结束 ZCode', 'danger sm', () => zcodeAction('kill', name))) : null));
  if (section === 'auth') return secPanel('登录管理', el('div', { class: 'cpt-settings' },
    info('登录平台', provider), info('登录账号', auth.account || name), info('凭据状态', loginState),
    auth.auth_status === 'locked' ? el('p', { role: 'status' }, '请在设置中解锁保管箱。') : null,
    el('div', { class: 'cpt-actions' },
      current || auth.has_auth ? zcodeOpenButton(instance) : null,
      btn('导入已保存登录', 'sm', () => openZcodeAdd({ existingName: name })),
      current && auth.has_auth ? btn('退出登录', 'danger sm', () => zcodeAction('zcode_logout', name)) : null),
    !auth.has_auth ? el('div', { class: 'cpt-actions' },
      btn('打开客户端登录', 'sm', () => zcodeAction('launch_app', name)),
      btn('保存当前登录', 'sm', () => zcodeAction('zcode_capture', name))) : null));
  if (section === 'data') {
    const directory = auth.native_data_dir || instance.data_dir || '';
    return secPanel('文件位置', el('article', { class: 'cpt-location' },
      el('h3', {}, '本机数据', qhelp('所有 ZCode 账号共用此处的对话和项目；移除卡片不会删除这些数据。')),
      el('code', { class: 'cpt-path' }, directory || '未记录'),
      el('div', { class: 'cpt-actions' },
        btn('打开文件夹', 'sm', () => run('open_dir', { app: 'zcode', name })),
        btn('复制路径', 'sm', async () => {
          try { await navigator.clipboard.writeText(directory); toast('已复制路径', true); }
          catch { toast('复制失败，请手动选择路径。', false); }
        }))));
  }
  if (section === 'danger') return secPanel('移除账号', el('div', { class: 'cpt-settings' },
    el('p', {}, '仅移除卡片。登录备份、本机对话和项目保留。'),
    el('div', { class: 'cpt-actions' }, btn('移除账号', 'danger sm', async () => {
      if (await del('zcode', name)) closeDetailNow();
    }))));
  return secPanel('账号概览', el('div', { class: 'cpt-settings' },
    el('div', { class: 'cpt-account-head' }, el('strong', {}, name),
      el('span', { class: `cpt-status${auth.has_auth ? ' is-ready' : ''}` }, current ? '当前账号' : loginState)),
    info('登录平台', provider), info('登录账号', auth.account || name),
    info('对话与项目', '共用本机数据'),
    el('div', { class: 'cpt-actions' }, zcodeOpenButton(instance), btn('修改名称', 'sm', () => renameAccount('zcode', name))),
    zcodeQuotaPanel(instance)));
}

function renderZcodeDetail(detail, instance) {
  $('im-save').hidden = true;
  if (!ZCODE_SECTIONS.some(([id]) => id === detail.sec)) detail.sec = 'info';
  const nav = $('im-nav'); clear(nav);
  for (const [id, label] of ZCODE_SECTIONS) {
    if (id === 'danger') nav.appendChild(el('li', { class: 'sep' }));
    const button = btn(label, id === 'danger' ? 'danger' : '', () => { detail.sec = id; detail.scroll = 0; renderDetail(); });
    button.setAttribute('aria-current', String(id === detail.sec));
    nav.appendChild(el('li', {}, button));
  }
  const body = $('im-body'); clear(body);
  body.appendChild(zcodeDetailPanel(instance, detail.sec));
  restoreScroll(body, detail.scroll || 0);
}

function openZcodeAdd(options = {}) {
  const overlay = el('div', { class: 'cpt-overlay' });
  const status = el('div', { class: 'hintline', role: 'status', aria: { live: 'polite' } });
  const select = el('select', { aria: { label: '已保存的 ZCode 登录' } });
  const name = el('input', { value: options.existingName || '', placeholder: '账号名称', aria: { label: 'ZCode 账号名称' } });
  let accounts = [], removed = false;
  const close = () => { removed = true; overlay.remove(); };
  const choose = () => {
    const account = accounts.find(a => a.user_id === select.value);
    if (account && !options.existingName) name.value = account.account || account.email || `${zcodeProvider(account.provider)} 账号`;
    submit.disabled = !account || (!!account.registered_as && account.registered_as !== options.existingName);
  };
  const refresh = async () => {
    status.textContent = '正在读取登录账号…'; submit.disabled = true;
    try {
      const result = await invoke('action', { payload: { action: 'zcode_accounts', app: 'zcode' } });
      if (removed) return;
      if (!result?.ok) throw new Error(result?.msg || '未能读取登录账号');
      accounts = (result.accounts || []).filter(a => a.has_auth);
      clear(select);
      for (const account of accounts) select.appendChild(el('option', { value: account.user_id },
        `${zcodeProvider(account.provider)} · ${account.account || account.email || '已保存账号'}${account.registered_as ? '（已登记）' : ''}`));
      const preferred = accounts.find(a => !a.registered_as || a.registered_as === options.existingName) || accounts[0];
      select.value = preferred?.user_id || '';
      status.textContent = accounts.length ? '' : '暂无已保存登录。请在 ZCode 中登录后刷新。';
      choose();
    } catch (error) { if (!removed) status.textContent = String(error?.message || error); }
  };
  select.addEventListener('change', choose);
  const submit = btn(options.existingName ? '绑定登录' : '添加账号', 'primary', async () => {
    if (!name.value.trim()) { status.textContent = '请输入账号名称。'; name.focus(); return; }
    submit.disabled = true;
    const result = await zcodeAction('zcode_capture', name.value.trim(), { user_id: select.value });
    if (result.ok) {
      close(); S.cur = 'zcode';
      if (options.existingName && S.detail) S.detail.sec = 'info';
      render();
    } else { status.textContent = result.msg || '未完成录入。'; choose(); }
  });
  submit.disabled = true;
  const dialog = el('div', { class: 'cpt-add', role: 'dialog', aria: { modal: 'true', label: '添加 ZCode 账号' } },
    el('h2', {}, options.existingName ? '绑定 ZCode 登录' : '添加 ZCode 账号',
      qhelp('BigModel 与 Z.ai 分别保存登录，使用同一份本机对话。添加和移除卡片不切换当前登录。')),
    el('label', {}, '已保存登录', select), el('label', {}, '账号名称', name), status,
    el('div', { class: 'cpt-actions' }, submit, btn('刷新登录列表', '', refresh), btn('取消', '', close)),
    btn('打开 ZCode 登录', 'sm', () => zcodeAction('launch_app', options.existingName || '')));
  overlay.appendChild(dialog); document.body.appendChild(overlay);
  refresh();
  return overlay;
}
