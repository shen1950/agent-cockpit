/* Keep the two Claude runtime IDs for actions and saved profiles. */
function isClaudeRuntime(app) {
  return app?.id === 'claude' || app?.id === 'claude-desktop';
}

function isLaunchTool(app) {
  if (!app) return false;
  const key = value => String(value || '').toLowerCase().replace(/[\s_-]/g, '');
  const names = ['agent驾驶舱', 'agentcockpit', 'magpie', 'cockpittools'];
  const id = key((app.id || '').replace(/^(?:auto|custom)-/, ''));
  const exe = (app.exe || '').split(/[\\/]/).pop().toLowerCase();
  return names.includes(id) || names.includes(key(app.name)) ||
    /^(?:agent-?cockpit|cockpit-tools|magpie(?:-windows-(?:amd64|arm64|386))?)\.exe$/.test(exe);
}

function productEntries(apps) {
  const visible = apps.filter(app => !app.hidden);
  const variants = visible.filter(isClaudeRuntime);
  if (variants.length < 2) return visible;
  const primary = variants.find(app => app.installed && app.id === 'claude') ||
    variants.find(app => app.installed) || variants[0];
  return visible.flatMap(app => !isClaudeRuntime(app) ? [app] : app === primary
    ? [{ ...primary, name: 'Claude', runtimeIds: variants.map(item => item.id) }] : []);
}

function capabilityLabel(app) {
  if (app.runtimeIds) return '命令行与桌面端';
  if (isLaunchTool(app)) return '启动工具';
  if (app.id === 'claude-desktop') return '桌面客户端';
  if (['zcode', 'antigravity'].includes(app.id)) return '账号切换';
  if (app.isolatable) return '支持多开';
  if (app.shared_profile || app.swap_ok) return '账号切换';
  return app.kind === 'cli' ? '命令行' : '桌面客户端';
}

/* 启动工具（Agent 驾驶舱、Magpie、Cockpit Tools 这类）：没有账号、没有额度，
   驾驶舱只负责把它叫起来。页面不摆空白，摆的是真能读到的信息——
   程序装在哪、多大、什么时候换过版、现在跑没跑着、跑的是哪些进程。 */
const LAUNCH_TOOL_PURPOSE = {
  agent驾驶舱: '驾驶舱自身。账号、额度、模型反代都在这个程序里管理。',
  magpie: '模型路由工具。模型与供应商由它自己管理，驾驶舱只负责启动。',
  cockpittools: '多 Agent 管理工具。账号与实例由它自己管理，驾驶舱只负责启动。',
};

function launchToolPurpose(app) {
  const key = String(app?.name || '').toLowerCase().replace(/[\s_-]/g, '');
  return LAUNCH_TOOL_PURPOSE[key] || '启动工具。账号与模型由它自己管理，驾驶舱只负责启动。';
}

function launchFileSize(bytes) {
  const n = Number(bytes);
  if (!Number.isFinite(n) || n <= 0) return '—';
  if (n >= 1024 * 1024 * 1024) return `${(n / 1024 / 1024 / 1024).toFixed(1)} GB`;
  if (n >= 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  if (n >= 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${n} 字节`;
}

function launchToolPanel(app, d) {
  const exe = app.exe || '';
  const procs = (d?.procs || []).filter((p) => p.app_id === app.id);
  const windows = procs.reduce((sum, p) => sum + (p.windows || 0), 0);
  const count = procs.reduce((sum, p) => sum + (p.count || 0), 0);
  const pids = procs.map((p) => p.pids).filter(Boolean).join('、');
  const detail = procs.map((p) => p.detail).filter(Boolean).join('；');

  const field = (label, text, control) => el('div', { class: 'setrow' },
    el('div', {}, el('div', { class: 'lab' }, label)),
    el('div', { class: 'ctl' }, control || el('span', { class: 'kv' }, text)));

  const info = el('div', { class: 'launch-block' },
    el('h3', {}, '程序信息'),
    field('程序名称', app.name),
    field('程序类别', '启动工具'),
    field('收录方式', app.discovered ? '自动发现' : (app.custom ? '手动添加' : '内置登记')),
    field('安装状态', app.installed ? '已安装' : '未找到可执行文件'),
    field('可执行文件', exe,
      exe ? el('div', { class: 'ctlrow wrap' },
        el('code', { class: 'launch-path' }, exe),
        btn('打开所在文件夹', 'sm', () => invoke('action', {
          payload: { action: 'reveal_dir', dir: exe.replace(/[\\/][^\\/]*$/, '') },
        }).catch(() => null), '在资源管理器里打开这个程序所在的文件夹'),
        btn('复制路径', 'sm', () => copyText(exe), '复制可执行文件的完整路径'))
        : el('span', { class: 'kv' }, '未找到可执行文件')),
    field('文件大小', launchFileSize(app.exe_size)),
    field('修改时间', app.exe_modified ? stampTime(app.exe_modified) : '—'),
    field('首次收录', app.first_seen ? stampTime(app.first_seen) : '—'),
    app.native_data_dir ? field('资料目录', app.native_data_dir) : null);

  const status = el('div', { class: 'launch-block' },
    el('h3', {}, '运行状态'),
    field('当前状态', count ? `运行中${windows ? `（${windows} 个窗口）` : ''}` : '未运行'),
    field('进程数量', count ? `${count} 个` : '0 个'),
    field('进程编号', pids || '—'),
    field('进程详情', detail || '当前没有该程序的进程'),
    el('div', { class: 'launch-actions' },
      btn('启动程序', 'primary', () => run('launch_app', { app: app.id }),
        '打开这个程序。账号与模型由它自己管理'),
      btn('刷新状态', 'sm', () => load(), '重新读取进程与文件信息')));

  return el('section', { class: 'panel launch-tool', data: { product: app.id } },
    el('div', { class: 'panelhead' },
      el('h2', {}, '启动工具'),
      el('span', { class: 'sub' }, launchToolPurpose(app))),
    el('div', { class: 'launch-tool-grid' }, info, status));
}

function renderClaudeRuntime(app, apps) {
  const host = $('claude-runtime');
  host.hidden = !isClaudeRuntime(app);
  if (host.hidden) return;
  clear(host);
  const choices = el('div', { class: 'runtime-choices', role: 'group', aria: { label: 'Claude 运行环境' } });
  for (const [id, label] of [['claude', 'Claude Code 命令行'], ['claude-desktop', 'Claude 桌面客户端']]) {
    if (!apps.some(a => a.id === id && !a.hidden)) continue;
    const button = btn(label, 'sm', () => { S.cur = id; render(); });
    button.setAttribute('aria-pressed', String(id === app.id));
    choices.append(button);
  }
  const cli = app.id === 'claude';
  const mode = cli && app.isolatable ? '各账号使用独立配置目录，可在多个终端同时运行。' :
    cli ? '通过命令行运行，实例能力取决于本机配置。' :
    '包含 Chat、Cowork 和 Code。账号在桌面客户端内切换，驾驶舱按单个客户端启动。';
  const routing = cli ? '可为本机默认配置或指定实例选择网关模型。' :
    '第三方网关使用一个选定的模型，断开后可恢复原配置。';
  host.append(choices, el('p', {}, mode + routing));
}

function profileStatus(instance) {
  const identity = instance.identity || {};
  if (identity.state === 'none') return '档案未初始化';
  if (identity.state === 'elsewhere') return '已建立，登录凭据由客户端单独保存';
  if (identity.state === 'logged') return '已检测到登录数据';
  if (identity.state === 'init') return '已建立，未检测到登录数据';
  return '未读取';
}
