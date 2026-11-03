/* Settings presentation. Links intentionally use the repository's real destinations. */
const COCKPIT_REPO = 'https://github.com/shen1950/agent-cockpit';
const SETTINGS_META = {
  profile: ['个人偏好', '个人资料', '为这台机器设置便于辨认的名称与备注。', 'M12 12a4 4 0 1 0 0-8 4 4 0 0 0 0 8 M4 21v-2a8 8 0 0 1 16 0v2'],
  general: ['', '常规', '管理启动、关闭窗口与日常刷新行为。', 'M4 7h16 M4 17h16 M8 4v6 M16 14v6'],
  appearance: ['', '外观', '选择适合你的主题、字号、密度与导航布局。', 'M12 3a9 9 0 1 0 9 9c0-2-3-1-5-2s-1-7-4-7 M7 10h.01 M9 6h.01 M6 14h.01'],
  quota: ['功能与数据', '额度提醒', '设置额度临期提醒，及时处理即将到期的资源。', 'M5 17h14l-2-3V9a5 5 0 0 0-10 0v5z M10 21h4'],
  ai: ['', 'AI 识别', '配置文本和截图识别使用的服务与模型。', 'M12 3l3 6 6 3-6 3-3 6-3-6-6-3 6-3z'],
  storage: ['', '存储', '查看数据位置、管理磁盘空间与迁移数据。', 'M4 5h16v14H4z M4 10h16 M8 15h3'],
  security: ['', '凭据安全', '管理加密保管箱与主密码。', 'M12 3l8 3v6c0 5-8 9-8 9s-8-4-8-9V6z M8 12l3 3 5-6'],
  advanced: ['维护与支持', '高级', '管理程序发现、兼容性与高级选项。', 'M4 7h16 M4 17h16 M15 4v6 M9 14v6'],
  logs: ['', '运行日志', '查看启动、退出与异常记录，帮助定位问题。', 'M6 3h9l4 4v14H6z M10 11h5 M10 15h5'],
  help: ['', '帮助中心', '从第一次添加账号，到解决日常使用问题。', 'M9 8a3 3 0 1 1 5 2c-2 1-2 2-2 4 M12 18h.01 M22 12a10 10 0 1 1-20 0 10 10 0 0 1 20 0'],
  about: ['', '关于与更新', '了解 AgentCockpit、查看发布记录与项目资源。', 'M12 11v6 M12 7h.01 M22 12a10 10 0 1 1-20 0 10 10 0 0 1 20 0'],
};
function settingsIcon(id) {
  const svg = svgNode('svg', { viewBox: '0 0 24 24', fill: 'none', stroke: 'currentColor', 'stroke-width': 1.7, 'stroke-linecap': 'round', 'stroke-linejoin': 'round', 'aria-hidden': 'true' });
  svg.append(svgNode('path', { d: SETTINGS_META[id][3] }));
  return svg;
}
function settingsNavigate(id) { setSection = id; renderSettings(); }
function settingsLink(label, path = '', cls = '') {
  const link = el('a', { class: `btn ${cls}`, href: COCKPIT_REPO + path, target: '_blank', rel: 'noopener noreferrer' }, label, el('span', { aria: { hidden: 'true' } }, ' ↗'));
  link.addEventListener('click', async (event) => {
    if (!window.__TAURI_INTERNALS__) return;
    event.preventDefault();
    try {
      const result = await invoke('action', { payload: { action: 'open_project_link', path } });
      if (!result.ok) toast(result.msg, false);
    } catch (error) { toast(`打开链接失败：${error}`, false); }
  });
  return link;
}
function renderSettingsNavigation(nav) {
  for (const [id] of SET_SECTIONS) {
    const [group, label] = SETTINGS_META[id];
    if (group) nav.append(el('li', { class: 'settings-group' }, group));
    const button = el('button', { type: 'button', data: { sec: id }, aria: { current: String(id === setSection) } }, settingsIcon(id), label);
    button.addEventListener('click', () => settingsNavigate(id));
    nav.append(el('li', {}, button));
  }
}
function settingsPageHeading(body) {
  const [, title, description] = SETTINGS_META[setSection];
  body.append(el('div', { class: 'settings-heading' }, el('h2', {}, title), el('p', {}, description)));
}
let helpQuery = '';
const helpExpanded = new Set();
function renderHelpCenter(body, panel) {
  body.append(panel('开始使用', el('p', { class: 'settings-copy' }, '选择 Agent → 添加账号 → 完成登录 → 查看额度。跟随引导熟悉主界面，或查阅完整使用手册。'),
    el('div', { class: 'settings-actions' }, btn('开始新手引导', 'primary', () => { S.view = 'home'; render(); startTour(); }), settingsLink('使用手册', '/blob/HEAD/docs/USER_MANUAL.md'))));
  const input = el('input', { type: 'search', value: helpQuery, placeholder: '搜索问题，例如：换号、额度、数据…', aria: { label: '搜索帮助' }, class: 'help-search' });
  const list = el('div', { class: 'help-questions' });
  const draw = () => {
    clear(list);
    const matches = HELP.filter(item => (item.t + item.h.replace(/<[^>]*>/g, '')).toLowerCase().includes(helpQuery.toLowerCase().trim()));
    for (const item of matches) {
      const answer = el('div', { class: 'html settings-answer' });
      answer.innerHTML = item.h; // static, bundled help text only
      const details = el('details', { open: helpExpanded.has(item.t) || !!helpQuery.trim() }, el('summary', {}, item.t), answer);
      details.addEventListener('toggle', () => { if (details.open) helpExpanded.add(item.t); else helpExpanded.delete(item.t); });
      list.append(details);
    }
    if (!matches.length) list.append(el('p', { class: 'settings-copy', role: 'status' }, '没有找到相关问题。试试其他关键词，或查看使用手册。'));
  };
  input.addEventListener('input', () => { helpQuery = input.value; draw(); });
  draw();
  body.append(panel('常见问题', input, list));
  body.append(panel('排查与反馈', el('p', { class: 'settings-copy' }, '遇到异常时，先查看运行日志。提交反馈请说明版本、操作步骤与实际结果；截图和日志请先移除账号、密钥等个人信息。'),
    el('div', { class: 'settings-actions' }, btn('查看运行日志', '', () => settingsNavigate('logs')), settingsLink('反馈问题', '/issues/new'), btn('重新检测多开能力', '', () => run('reset_caps'), '清除检测记录，下次启动账号时重新判断'))));
}
function githubMark() {
  const svg = svgNode('svg', { viewBox: '0 0 24 24', fill: 'currentColor', 'aria-hidden': 'true' });
  svg.append(svgNode('path', { d: 'M12 .3a12 12 0 0 0-3.8 23.4c.6.1.8-.3.8-.6v-2.3c-3.3.7-4-1.4-4-1.4-.5-1.4-1.3-1.7-1.3-1.7-1.1-.8.1-.8.1-.8 1.2.1 1.8 1.2 1.8 1.2 1.1 1.8 2.8 1.3 3.5 1 .1-.8.4-1.3.8-1.6-2.7-.3-5.5-1.3-5.5-5.9 0-1.3.5-2.4 1.2-3.2-.1-.3-.5-1.5.1-3.2 0 0 1-.3 3.3 1.2a11.4 11.4 0 0 1 6 0C17.3 4.9 18.3 5.2 18.3 5.2c.6 1.7.2 2.9.1 3.2.8.8 1.2 1.9 1.2 3.2 0 4.6-2.8 5.6-5.5 5.9.4.4.8 1.1.8 2.2v3.4c0 .3.2.7.8.6A12 12 0 0 0 12 .3z' }));
  return svg;
}
function renderAboutSettings(body, panel) {
  const version = S.data.app_version;
  const repoLink = settingsLink('shen1950 / agent-cockpit', '', 'about-repo');
  repoLink.prepend(githubMark());
  body.append(el('section', { class: 'about-hero' }, el('img', { src: brandUrl(S.data.settings?.appearance), data: { 'brand-icon': '' }, alt: 'AgentCockpit 应用图标', width: 104, height: 104 }),
    el('h2', {}, 'AgentCockpit'), el('p', { class: 'about-name' }, 'Agent 驾驶舱'),
    el('span', { class: 'about-version' }, version ? `版本 ${version}` : '版本信息未提供'),
    el('p', { class: 'settings-copy' }, '让 AI 工具、账号与额度井井有条。'), repoLink,
    el('div', { class: 'settings-actions' }, settingsLink('查看最新版本', '/releases/latest', 'primary'), settingsLink('更新日志', '/releases')),
    el('p', { class: 'settings-copy about-note' }, '在 GitHub 发布页查看版本与下载文件。')));
  body.append(el('div', { class: 'about-resources' },
    panel('文档与支持', el('div', { class: 'settings-actions' }, btn('帮助中心', '', () => settingsNavigate('help')), settingsLink('使用手册', '/blob/HEAD/docs/USER_MANUAL.md'), settingsLink('问题反馈', '/issues'))),
    panel('许可与致谢', el('p', { class: 'settings-copy' }, 'MIT License · © 2026 shen1950'), el('p', { class: 'settings-copy' }, '基于 Tauri、Rust 与 WebView2 构建。感谢开源社区与所有参与反馈的用户。'), settingsLink('查看许可证', '/blob/HEAD/LICENSE'))));
  body.append(panel('本机信息', row('应用版本', '反馈问题时可复制此信息，不包含账号与密钥。', btn('复制版本信息', 'sm', () => copyText(`AgentCockpit\n版本：${version || '未知'}\n桌面框架：Tauri 2 / WebView2`))),
    row('数据目录', S.data.data_root || '~/.agent-cockpit/', btn('打开目录', 'sm', () => run('open_data_dir'))),
    row('数据与隐私', '账号配置保存在本机，凭据由加密保管箱保护。AI 识别与额度查询会连接你配置的服务。', btn('凭据安全', 'sm', () => settingsNavigate('security')))));
  body.append(panel('法律与免责',
    el('div', { class: 'legalabout' },
      ...LEGAL_POINTS.map(([title, text]) => el('div', { class: 'legalabout-item' }, el('b', {}, title), el('span', {}, text)))),
    el('p', { class: 'legal-fine' }, LEGAL_ONE_LINE),
    el('div', { class: 'settings-actions' },
      legalLink('完整免责条款', LEGAL_DOC_PATH),
      legalLink('反代风险说明', PROXY_DOC_PATH),
      btn('重新查看反代协议', 'sm', async () => {
        if (await ensureProxyConsent(true)) toast('已重新确认反代协议', true);
      }, '再看一遍启用反代前需要同意的条款'))));
}
