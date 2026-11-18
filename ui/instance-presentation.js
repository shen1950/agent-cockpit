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
