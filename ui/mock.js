/* 浏览器预览用假桥接（Tauri 环境不会加载此文件）
 * 直接 `python -m http.server` 后打开 index.html 即可目检浅色/深色两套主题与窄窗布局。
 */
const now = Date.now() / 1000;
const inst = (app, app_name, name, kind, running, procs, q, extra = {}) => ({
  app, app_name, kind, name, running, procs,
  workdir: 'D:\\Claude\\人工智能产品测试',
  created: '2026-09-28',
  swapped: false,
  identity: running
    ? { state: 'logged', label: '有登录数据（3 项标记 · 共 1204 文件）', files: 1204, product: app }
    : (extra.idle === 'none'
      ? { state: 'none', label: '从未启动过 · 点「启动并登录」后在窗口里登录', files: 0, product: '' }
      : { state: 'init', label: '已写入 241 个文件，未见登录痕迹', files: 241, product: '' }),
  usage: extra.usage ?? null,
  usage_fetched_at: extra.usage_fetched_at ?? null,
  quota: { total: null, left: null, expires_on: '', days_left: null, has_token: false, quota_url: false, adapter: '', fetched_at: null, credits: [], groups: [], plan_name: '', source: '', stale: false, live_error: '', auth_from: '', detail_source: '', detail_at: null, detail_stale: false, web_error: '', trend: null, ...q },
  ...extra,
});

/* WorkBuddy 那种"一个大组底下几十笔各自到期"的形状（agenthub 从 free/paid-packages 拿回来的） */
// 2026-10-11 真从 qoder.com.cn/account/usage 抓回来的形状：24 个 100 积分的赠送包，
// 各自一个到期时间——这正是"本机接口看不到、只有官网有"的那一层。
/* WorkBuddy 按天消耗：真从 ~/.workbuddy/workbuddy.db 的 session_usage 里聚合出来的
   31 天（2026-06-20 ~ 2026-10-12，合计 10550.30 credits，峰值 9-09 那天 2668.24），
   不是编的样例——界面预览要能跟 agenthub 实测数字逐位对上。 */
function wbTrend() {
  const rows = [['2026-06-20', 178.97, 15], ['2026-06-26', 1.41, 5], ['2026-07-20', 3.86, 1], ['2026-07-21', 364.41, 3], ['2026-07-22', 1340.1, 3], ['2026-07-26', 3.52, 1], ['2026-07-27', 860.72, 4], ['2026-07-30', 49.79, 1], ['2026-07-31', 158.13, 1], ['2026-08-02', 574.33, 1], ['2026-08-18', 41.84, 1], ['2026-08-19', 59.95, 1], ['2026-08-25', 586.05, 4], ['2026-08-27', 322.99, 3], ['2026-08-28', 28.92, 1], ['2026-08-29', 121.82, 2], ['2026-09-02', 176.68, 1], ['2026-09-04', 102.67, 1], ['2026-09-09', 2668.24, 27], ['2026-09-13', 1200.34, 3], ['2026-09-25', 30.48, 3], ['2026-09-26', 73.37, 4], ['2026-09-27', 29.96, 2], ['2026-09-28', 30.88, 2], ['2026-09-30', 18.16, 1], ['2026-10-03', 0.21, 1], ['2026-10-04', 1.04, 1], ['2026-10-07', 76.73, 2], ['2026-10-09', 498.59, 6], ['2026-10-10', 941.27, 1], ['2026-10-12', 4.87, 1]];
  return {
    unit: 'credits',
    total: 10550.3,
    peak: 2668.24,
    peak_date: '2026-09-09',
    from: rows[0][0],
    to: rows[rows.length - 1][0],
    db: 'C:\\Users\\LENOVO\\.workbuddy\\workbuddy.db',
    days: rows.map((r) => ({ date: r[0], credits: r[1], sessions: r[2] })),
  };
}

/* Qoder 那条官网热力图接口的真实形状（2026-10-12 实测）：厂商自己把 371 天补零成连续
   序列、带 unit/total/levels 四档阈值，items[] 只有 date+value，**没有场次数**。
   数值用实测到的那两天，其余按形状补零——够验"厂商分档"和"没有会话数时不许编"这两条。 */
function qoderTrend() {
  const pad = (x) => String(x).padStart(2, '0');
  const today = new Date();
  today.setHours(0, 0, 0, 0);
  const days = [];
  for (let i = 370; i >= 0; i--) {
    const dt = new Date(today);
    dt.setDate(dt.getDate() - i);
    days.push({ date: `${dt.getFullYear()}-${pad(dt.getMonth() + 1)}-${pad(dt.getDate())}`, credits: 0 });
  }
  const known = { '2026-10-05': 36.0456504078, '2026-10-10': 1190.5914247282, '2026-10-11': 206.69 };
  for (const d of days) if (known[d.date] != null) d.credits = known[d.date];
  return {
    unit: 'credits', total: 9836.16, peak: 1190.5914247282, peak_date: '2026-10-10',
    levels: [29.35, 206.69, 879.14, 1190.59], from: days[0].date, to: days[days.length - 1].date,
    source: '官网热力图接口', days,
  };
}

function webGroups() {
  const at = (days, hh, mm) => {
    const d = new Date((now + days * 86400) * 1000);
    const z = (v) => String(v).padStart(2, '0');
    return `${d.getFullYear()}-${z(d.getMonth() + 1)}-${z(d.getDate())} ${z(hh)}:${z(mm)}`;
  };
  const bonus = [];
  for (let i = 0; i < 24; i++) {
    const used = i < 4 ? 100 : (i === 4 ? 49 : 0);
    bonus.push({
      label: '平台赠送', pool: '平台奖励积分', detail: 'RESOURCE_PACKAGE_SOURCE_BONUS',
      total: 100, used, left: 100 - used, unit: 'credits',
      expires_on: at(i - 3, 9 + (i % 9), (i * 7) % 60),
      days_left: i - 3, expired: i < 3,
    });
  }
  bonus.sort((a, b) => a.expires_on.localeCompare(b.expires_on));
  return [
    {
      name: '套餐基础积分', unit: 'credits', left: 0, total: 0, used: 0,
      live_count: 1, expired_count: 0, live_left: 0, expires: '', days_left: null,
      items: [{
        label: '套餐额度', pool: '套餐基础积分', detail: 'PLAN', total: 0, used: 0, left: 0,
        unit: 'credits', expires_on: '未给出', days_left: null, expired: false,
      }],
    },
    {
      name: '平台奖励积分', unit: 'credits', left: 1751, total: 2400, used: 649,
      live_count: 21, expired_count: 3, live_left: 1751,
      expires: bonus[3].expires_on, days_left: 0,
      items: bonus,
    },
  ];
}
function wbGroups() {
  const at = (days, hh, mm) => {
    const d = new Date((now + days * 86400) * 1000);
    const z = (v) => String(v).padStart(2, '0');
    return `${d.getFullYear()}-${z(d.getMonth() + 1)}-${z(d.getDate())} ${z(hh)}:${z(mm)}`;
  };
  const reward = [];
  for (let i = 0; i < 9; i++) {
    reward.push({
      label: 'CodeBuddy个人版国内运营裂变包', pool: '资源包 TCACA_code_007_nzdH5h4Nl',
      detail: '官方活动发放', total: [10, 100, 50, 9, 100, 7, 100, 8, 26][i],
      used: [10, 100, 50, 9, 40, 0, 100, 0, 26][i],
      left: [0, 0, 0, 0, 60, 7, 0, 8, 0][i],
      unit: 'credits', expires_on: at(i - 2, 15, 31 + i),
      days_left: i - 2, expired: i < 2,
    });
  }
  reward.push({
    label: 'CodeBuddy个人版拉新权益包', pool: '资源包 TCACA_code_030_BjSt89qTvr',
    detail: '官方活动发放', total: 1000, used: 0, left: 1000, unit: 'credits',
    expires_on: at(51, 15, 12), days_left: 51, expired: false,
  });
  return [
    {
      name: '套餐基础积分', unit: 'credits', left: 500, total: 500, used: 0,
      live_count: 1, expired_count: 0, live_left: 500,
      expires: at(20, 23, 59), days_left: 20,
      items: [{
        label: 'CodeBuddy个人体验版', pool: '资源包 TCACA_code_008_cfWoLwvjU', detail: '',
        total: 500, used: 0, left: 500, unit: 'credits', expires_on: at(20, 23, 59),
        days_left: 20, expired: false,
      }],
    },
    {
      name: '平台奖励积分', unit: 'credits', left: 3694.84000019, total: 4443, used: 748.15999981,
      live_count: 8, expired_count: 2, live_left: 3694.84,
      expires: at(0, 15, 31), days_left: 0,
      items: reward,
    },
  ];
}

const DATA = {
  apps: [
    { id: 'qoder', name: 'Qoder', kind: 'electron', installed: true, swapped: '', swap_ok: true },
    { id: 'qoder-cn', name: 'Qoder CN', kind: 'electron', installed: true, swapped: '账号2', swap_ok: true,
      isolatable: false,
      local_quota: true, web_quota: true, auth_switch: true,
      machine: { known: true, refreshing: false, value: {
        ok: true, account: 'shen1950', user_id: '019e8247-2e21-7aa6-9e87-b0720003b387',
        user_type: 'personal_standard', left: 1751, total: 2400, expires_on: '未给出',
        source: 'server', auth_from: 'C:\\Users\\LENOVO\\AppData\\Roaming\\com.qodercn.app.stable\\auth.v1.dat',
        profile_dir: 'C:\\Users\\LENOVO\\AppData\\Roaming\\com.qodercn.app.stable' } } },
    { id: 'qoder-cn-ide', name: 'Qoder CN IDE', kind: 'electron', installed: true, swapped: '', swap_ok: true },
    { id: 'workbuddy', name: 'WorkBuddy', kind: 'electron', installed: true, swapped: '', swap_ok: true },
    { id: 'traework-cn', name: 'TRAE CN', kind: 'electron', installed: true, swapped: '', swap_ok: true },
    { id: 'zcode', name: 'ZCode', kind: 'electron', installed: true, swapped: '', swap_ok: true },
    { id: 'codex', name: 'Codex CLI', kind: 'cli', installed: true, swapped: '', swap_ok: true },
    { id: 'claude', name: 'Claude Code CLI', kind: 'cli', installed: false, swapped: '', swap_ok: true },
    { id: 'auto-deepseekharness', name: 'DeepSeek Harness', kind: 'electron', installed: true, swapped: '', swap_ok: true, discovered: true, isolatable: true, short: 'DSH' },
    { id: 'auto-ollama', name: 'Ollama', kind: 'plain', installed: true, swapped: '', swap_ok: false, discovered: true, isolatable: false, short: 'OL' },
  ],
  instances: [
    // 演示"读到旧档案"这个失败态：现在正常情况会读到 0.4.x 新壳的
    // %APPDATA%\com.qoder.app.stable\auth.v1.dat（下面账号2 就是），
    // 只有当机器上只剩旧 VS Code 壳那份 8 月的 state.vscdb 时才会长成账号1 这样
    inst('qoder', 'Qoder', '账号1', 'electron', true, 12, {
      total: 500, left: 500, expires_on: '2026-08-28', days_left: -41,
      local_account: 'Brilliant Liare〈rl1950790741@163.com〉',
      auth_from: 'C:\\Users\\LENOVO\\AppData\\Roaming\\Qoder\\User\\globalStorage\\state.vscdb',
      source: 'local-cache', stale: true,
      live_error: 'HTTP 401 {"code": "TOKEN_EXPIRE", "message": "token is not active"}',
      fetched_at: now - 700,
      credits: [
        { label: '主额度', left: 0, total: 0, expires_on: '无限期', days_left: null, note: '' },
        { label: '资源包 act-20260826-990', left: 500, total: 500, expires_on: '2026-08-28', days_left: -41, note: '' },
      ],
    }),
    inst('qoder', 'Qoder', '账号2', 'electron', false, 0, {
      total: 1200, left: 1200, expires_on: '无限期', days_left: null,
      source: 'server',
      auth_from: 'C:\\Users\\LENOVO\\AppData\\Roaming\\com.qoder.app.stable\\auth.v1.dat',
    }),
    // 演示"本机已经换成另一个号"：这张卡已让位，只留历史额度。
    // 明细是两小时前从官网读回来的 24 笔，本机那条接口只有两个汇总数——守卫让它只刷汇总数，
    // 不许把明细压回一行，所以这里就是修完之后卡片该长的样子（大字新、明细旧且自己说明）
    inst('qoder-cn', 'Qoder CN', '账号1', 'electron', false, 0, {
      total: 4100, left: 4000, expires_on: '2026-10-20', days_left: 9, quota_url: true, fetched_at: now - 5000,
      local_account: 'nick9994374911', user_id: '019e0000-0000-0000-0000-000000000001',
      bound_mismatch: true, source: 'server',
      groups: webGroups(), detail_source: 'web', detail_at: now - 2 * 3600, detail_stale: true,
    }, { released: true, released_to: 'shen1950', origin: 'adopt', app_running: true,
      auth_pack: {
        at: now - 7200, account: 'nick9994374911',
        user_id: '019e0000-0000-0000-0000-000000000001',
        dir: 'C:\\Users\\LENOVO\\AppData\\Roaming\\com.qodercn.app.stable',
        files: [{ name: 'auth.v1.dat', bytes: 414 }],
      } }),
    // 演示官网逐资源包读数：三个大组 + 组内逐笔各带自己的到期时间
    // 共用一份档案的产品：整棵进程树记在"现在登着的那个号"头上，不是磁盘换号记录里那个
    inst('qoder-cn', 'Qoder CN', 'shen1950', 'electron', true, 8, {
      total: 2400, left: 1751, expires_on: '2026-10-24 21:13', days_left: 13,
      local_account: 'shen1950', user_id: '019e8247-2e21-7aa6-9e87-b0720003b387',
      source: 'web', plan_name: '体验版', fetched_at: now - 60,
      groups: webGroups(), detail_source: 'web', detail_at: now - 60, trend: qoderTrend(),
    }),
    inst('qoder-cn', 'Qoder CN', '账号2', 'electron', false, 0, {}, { swapped: true }),
    inst('workbuddy', 'WorkBuddy', '账号1', 'electron', false, 0, {
      total: 4943, left: 4194.84000019, expires_on: '2026-10-19', days_left: 8,
      source: 'server', fetched_at: now - 900, plan_name: '体验版',
      local_account: 'DuckDeity',
      auth_from: 'C:\\Users\\LENOVO\\AppData\\Local\\CodeBuddyExtension\\Data\\Public\\auth\\workbuddy-desktop.info',
      groups: wbGroups(), trend: wbTrend(),
    }),
    inst('traework-cn', 'TRAE CN', '账号1', 'electron', true, 6, { total: 800, left: 760, expires_on: '2026-11-02', days_left: 32 }),
    // 演示：这个账号的数据是"接管"进来的（对话与记忆都在里面）
    inst('zcode', 'ZCode', '账号1', 'electron', false, 0, {}, {
      origin: 'adopt',
      seeded_from: 'C:\\Users\\LENOVO\\.zcode.cockpit-original',
    }),
    inst('codex', 'Codex CLI', '账号1', 'cli', true, 1, { total: 100, left: 63, expires_on: '2026-10-30', days_left: 29, has_token: true }, {
      usage: { input: 48_210_533, output: 3_102_884, cached: 39_551_200, reasoning: 1_830_117, total: 51_313_417, files: 63 },
      usage_fetched_at: now - 900,
    }),
  ],
  procs: [
    { app: 'Qoder', app_id: 'qoder', detail: '12 个进程（含渲染/GPU 子进程）', pids: '8400,11200,21276,24404,24500,29496', count: 12 },
    { app: 'Qoder CN', app_id: 'qoder-cn', detail: '8 个进程（含渲染/GPU 子进程）', pids: '3011,3020,3155,3400', count: 8 },
    { app: 'Codex CLI / 账号1', app_id: 'codex', detail: 'D:\\Claude\\人工智能产品测试', pids: '17820', count: 1 },
  ],
  alerts: [
    // 旧缓存不产生"临期"提醒（后端 state.rs 里 quota_stale 会跳过），所以这里也不列 Qoder
    { app: 'TRAE CN', app_id: 'traework-cn', name: '账号1', days: 32, left: 760, level: 'warn' },
    { app: 'Codex CLI', app_id: 'codex', name: '账号1', days: 29, left: 63, level: 'warn' },
    { app: 'Qoder CN', app_id: 'qoder-cn', name: '账号1', days: 23, left: 120, level: 'warn' },
  ],
  stats: { instances: 8, running: 4, running_instances: 3, agents: 10, auto: 2 },
  scan: { auto: 2, at: '15:20' },
  proxy: {
    running: true, port: 8788, upstream: 'https://api.openai.com', strategy: 'round-robin',
    pool: ['主力池 · Qoder/账号1', '主力池 · Qoder/账号2', '备用池 · Key/Longcat/Pot'],
    /* 反代编制：来源 → 池子 → 分发 Key */
    pools: [
      {
        id: 'gp-1', name: '主力池', enabled: true, live: 2, labels: ['Qoder/账号1', 'Qoder/账号2'],
        members: [
          { src: 'instance:qoder/账号1', base_url: '', enabled: true },
          { src: 'instance:qoder/账号2', base_url: '', enabled: true },
          { src: 'instance:workbuddy/账号1', base_url: '', enabled: true },   // 没 Token → 不计入可用
        ],
      },
      {
        id: 'gp-2', name: '备用池', enabled: true, live: 1, labels: ['Key/Longcat/Pot'],
        members: [{ src: 'key:p-longcat/k-lc-1', base_url: '', enabled: true }],
      },
      {
        id: 'gp-3', name: '便宜中转（观察中）', enabled: false, live: 0, labels: [],
        members: [],
      },
    ],
    dist: [
      {
        id: 'dk-1', name: '给 Pot', masked: 'ac-3f2a••••9c41', pools: ['gp-1'], enabled: true,
        calls: 1284, last_used: Math.floor(now) - 96, last_account: 'Qoder/账号1',
      },
      {
        id: 'dk-2', name: '团队小王', masked: 'ac-88b1••••02de', pools: ['gp-1', 'gp-2'], enabled: true,
        calls: 37, last_used: Math.floor(now) - 9000, last_account: 'Key/Longcat/Pot',
      },
      {
        id: 'dk-3', name: '沉浸式翻译（已停用）', masked: 'ac-5c07••••7a13', pools: [], enabled: false,
        calls: 906, last_used: Math.floor(now) - 260000, last_account: 'Qoder/账号2',
      },
    ],
    members: {
      instances: [
        { src: 'instance:qoder/账号1', app: 'qoder', app_name: 'Qoder', instance: '账号1', has_token: true, proxy_base_url: '' },
        { src: 'instance:qoder/账号2', app: 'qoder', app_name: 'Qoder', instance: '账号2', has_token: true, proxy_base_url: '' },
        { src: 'instance:qoder-cn/账号1', app: 'qoder-cn', app_name: 'Qoder CN', instance: '账号1', has_token: true, proxy_base_url: '' },
        { src: 'instance:qoder-cn/账号2', app: 'qoder-cn', app_name: 'Qoder CN', instance: '账号2', has_token: false, proxy_base_url: '' },
        { src: 'instance:workbuddy/账号1', app: 'workbuddy', app_name: 'WorkBuddy', instance: '账号1', has_token: false, proxy_base_url: '' },
        { src: 'instance:codex/账号1', app: 'codex', app_name: 'Codex CLI', instance: '账号1', has_token: true, proxy_base_url: '' },
      ],
      keys: [
        { src: 'key:p-longcat/k-lc-1', site_id: 'p-longcat', site_name: 'Longcat', base_url: 'https://api.longcat.chat/openai/v1', label: 'Pot', masked: 'sk-lc-••••1111' },
        { src: 'key:p-longcat/k-lc-3', site_id: 'p-longcat', site_name: 'Longcat', base_url: 'https://api.longcat.chat/anthropic', label: 'Anthropic 格式接入', masked: 'sk-lc-••••3333' },
      ],
    },
    logs: [
      { ts: '23:41:08', method: 'POST', path: '/v1/chat/completions', model: 'gpt-4o-mini', status: 200, account: 'Qoder/账号1', pool: '主力池', via: '给 Pot（主力池）' },
      { ts: '23:40:52', method: 'POST', path: '/v1/chat/completions', model: 'gpt-4o', status: 429, account: 'Codex CLI/账号1', pool: '主力池', via: '给 Pot（主力池）' },
      { ts: '23:39:14', method: 'GET', path: '/v1/models', model: '-', status: 200, account: 'Key/Longcat/Pot', pool: '备用池', via: '团队小王（主力池、备用池）' },
    ],
  },
  now: new Date().toLocaleTimeString('zh-CN', { hour12: false }),
};

DATA.apps.forEach((a) => {
  if (a.isolatable === undefined) a.isolatable = a.kind !== 'plain';
  // 后端 state.rs 的 shared_profile：账号全共用默认位置那一份档案，切号＝客户端里退出登录再登录
  if (a.shared_profile === undefined) a.shared_profile = a.kind === 'electron' && !a.isolatable;
  if (a.icon === undefined) a.icon = null;
  if (a.discovered === undefined) a.discovered = false;
  if (a.removed === undefined) a.removed = a.discovered && !a.installed;
  if (a.new === undefined) a.new = false;
  if (a.exe === undefined) a.exe = 'D:/apps/' + a.id + '/' + a.name + '.exe';
});
// state.rs 的 instances[].logged_in：卡上绑的 userId 就是本机当前登录那个
DATA.instances.forEach((x) => {
  const a = DATA.apps.find((y) => y.id === x.app) || {};
  const mv = (a.machine && a.machine.value) || {};
  if (x.logged_in === undefined) {
    x.logged_in = !!(mv.ok && mv.user_id && x.quota && x.quota.user_id === mv.user_id);
  }
});
DATA.hidden_apps = [{ id: 'auto-skinbox', name: '皮肤盒子', discovered: true, exe: 'D:/Artificial_Intelligence/skinbox/skin.exe' }];

/* 两级结构：站点 → 该站下的多个 key（每个 key 有各自 base_url / 模型 / 用途标签） */
DATA.keys = [
  {
    id: 'p-longcat', name: 'Longcat', kind: 'official', homepage: 'https://longcat.chat',
    key_count: 3,
    left: 68.4, total: 100,   // 额度挂在站点上，key 行不再各显示一份
    keys: [
      { id: 'k-lc-1', label: 'Pot', key: 'sk-lc-aaaaaaaaaaaaaaa1111',
        masked: 'sk-lc-••••1111', base_url: 'https://api.longcat.chat/openai/v1',
        models_list: 'LongCat-2.0, LongCat-2.5-Preview', models: 2,
        left: 68.4, total: 100, expires_on: '2026-12-31', days_left: 86,
        last_status: 'ok', last_msg: '可用（列出 2 个模型）', last_check: '10-06 14:22',
        use_in_proxy: true, note: '主力' },
      { id: 'k-lc-2', label: 'WorkBuddy', key: 'sk-lc-bbbbbbbbbbbbbbb2222',
        masked: 'sk-lc-••••2222', base_url: 'https://api.longcat.chat/anthropic',
        models_list: 'LongCat-2.5-Preview', models: 1,
        left: 68.4, total: 100, expires_on: '2026-12-31', days_left: 86,
        last_status: 'ok', last_msg: '可用（列出 1 个模型）', last_check: '10-06 14:22',
        use_in_proxy: false, note: 'Anthropic 格式接入' },
      { id: 'k-lc-3', label: 'QoderCN', key: 'sk-lc-cccccccccccccc3333',
        masked: 'sk-lc-••••3333', base_url: 'https://api.longcat.chat/openai/v1',
        models_list: 'LongCat-2.0', models: 1,
        left: 12.0, total: 100, expires_on: '2026-10-12', days_left: 6,
        last_status: 'ok', last_msg: '可用（列出 1 个模型）', last_check: '10-06 14:20',
        use_in_proxy: false, note: '临时用过' },
    ],
  },
  {
    id: 'p-bailian', name: '阿里云百炼', kind: 'official',
    homepage: 'https://bailian.console.aliyun.com', key_count: 1,
    base_urls: [
      { format: 'openai', url: 'https://ws-rw0go8glwhz1cu9v.cn-beijing.maas.aliyuncs.com/compatible-mode/v1' },
      { format: 'anthropic', url: 'https://ws-rw0go8glwhz1cu9v.cn-beijing.maas.aliyuncs.com/apps/anthropic' },
      { format: 'other', url: 'https://ws-rw0go8glwhz1cu9v.cn-beijing.maas.aliyuncs.com/api/v1' },
    ],
    models_list: 'qwen3.8-flash, glm-5.3-flash, deepseek-v4-pro, kimi-k3',
    keys: [
      { id: 'k1', label: '主用', key: 'sk-ws9-demo-key-3fA2',
        masked: 'sk-ws9••••3fA2', base_url: 'https://dashscope.aliyuncs.com/compatible-mode/v1',
        models_list: 'qwen3-max, qwen-plus', models: 42,
        total: 1000000, left: 812000, expires_on: '2026-12-31', days_left: 86,
        last_status: 'ok', last_msg: '可用（列出 42 个模型）', last_check: '10-02 14:22',
        quota_adapter: 'openai', quota_url: '', quota_path: '', fetched_at: now - 1200,
        use_in_proxy: true, note: '主用 qwen3-max' },
    ],
  },
  {
    id: 'p-relay-a', name: '某中转站 A', kind: 'relay', key_count: 1,
    keys: [
      { id: 'k2', label: '备用', key: 'sk-rel-demo-9c1d',
        masked: 'sk-rel••••9c1d', base_url: 'https://api.relay-a.example/v1',
        models_list: 'gpt-5, claude-sonnet-4-5', models: 118,
        total: 50, left: 6.4, expires_on: '2026-10-12', days_left: 6,
        last_status: 'ok', last_msg: '可用（列出 118 个模型）', last_check: '10-02 09:11',
        use_in_proxy: true, note: '按量计费，别忘' },
    ],
  },
  {
    id: 'p-ollama', name: '本地 Ollama', kind: 'local', key_count: 1,
    keys: [
      { id: 'k3', label: '本地', key: 'ollama',
        masked: '••••', base_url: 'http://127.0.0.1:11434/v1',
        models_list: 'qwen3:8b', models: 3,
        total: null, left: null, expires_on: '', days_left: null,
        last_status: '', last_msg: '', last_check: '', use_in_proxy: false, note: '' },
    ],
  },
  {
    id: 'p-zhipu', name: '智谱 GLM', kind: 'official', key_count: 1,
    keys: [
      { id: 'k4', label: '赠送', key: 'sk-zp7-demo-41ba',
        masked: 'sk-zp7••••41ba', base_url: 'https://open.bigmodel.cn/api/paas/v4',
        models_list: 'glm-4.6', models: 0,
        total: 20, left: 0, expires_on: '2026-09-20', days_left: -16,
        last_status: 'fail', last_msg: 'HTTP 429', last_check: '10-01 22:40',
        use_in_proxy: false, note: '赠送已用尽' },
    ],
  },
];

DATA.settings = {
  profile: { nickname: '', note: '' },
  general: { autostart: false, close_action: 'tray', notify_alerts: true, refresh_sec: 4, auto_scan: true },
  appearance: { theme: 'dark', density: 'comfortable', font_scale: 100, nav_w: 248, nav_h: 0, nav_collapsed: false },
  quota: { warn_days: 30, danger_days: 7, auto_fetch_min: 30 },
  proxy: { port: 8788, upstream: 'https://api.openai.com', log_keep: 100 },
  checkin: { enabled: false, window_start: 6, window_end: 12, auto: { workbuddy: true },
    remind: true, remind_at: '10:05', remind_mute: {} },
};
/* 六家今天的状态：2026-10-12 用本机凭据只读查到的真实返回，不是编的样例 */
const CI_ITEMS = [
  { product: 'workbuddy', account: 'DuckDeity', supported: true, auto: true, activity_on: true,
    signed_today: true, claimable: false, streak: 13, reward: 100, today_credit: 100,
    total: 1300, days_signed: 13, ends_on: '2026-10-15', unit: '积分',
    source: 'https://www.codebuddy.cn/v2/billing/meter/checkin-activity-status' },
  { product: 'workbuddy-activity', supported: true, auto: true, activity_on: true, mode: 'activity',
    signed_today: false, claimable: true, streak: 100, unit: '活跃',
    words: { done: '今天已打卡', go: '今天还没打卡', act: '发一句对话', done_act: '今天已领完' },
    note: '活跃地图要「完成一次对话」才记录当天（连登 14 天可兑换奖励）；这一条只管打卡，签到是上面 WorkBuddy 那条' },
  { product: 'workbuddy-travel', supported: true, auto: true, activity_on: true, mode: 'travel',
    signed_today: false, claimable: true, travel_state: 'traveling', next_action: 'wait',
    reward: 10, unit: '积分', arrive_at: '10-15 18:20',
    words: { done: '今天已领取', go: '今天可派可领', depart: '派出', wait: '等待中',
      claim: '领取', wait_badge: '猫猫在路上', done_act: '今天已领完' },
    note: '1~4 小时一趟、每天一趟；先派、到点再领（这一趟约 10-15 18:20 到）' },
  { product: 'workbuddy-intl', account: 'shen1950', supported: true, auto: true, activity_on: true,
    mode: 'activity', signed_today: false, claimable: true, streak: 0, reward: 30, unit: '积分',
    words: { done: '今天已到账', go: '今天还没到账', act: '发一句对话', done_act: '今天已领完' },
    note: '30 积分/天是服务端按天结算的基础权益（历史发放时刻 00:20~08:02 不等）' },
  { product: 'trae-cn', account: 'shen195', supported: true, auto: true, activity_on: true,
    signed_today: true, claimable: false, reward: 100, extra: 100, unit: '积分' },
  { product: 'minimax', supported: true, auto: true, activity_on: true, signed_today: true,
    claimable: false, streak: 6, reward: 400, unit: '积分' },
  // MiniMax 在这台机器上有两个号，但登录态整机一份——账号行只能是占位行（shared），
  // 这正是 2026-10-17 要求的"通用管理"：不是只有 Qoder CN 才展开
  { product: 'minimax', scope: 'account', parent: 'minimax', account: 'acct-A',
    supported: true, auto: false, activity_on: true, signed_today: null, claimable: false,
    shared: true, unit: '积分',
    note: '这个产品的登录态是整机一份，换号会覆盖前一个；签到状态要在切到该号之后才读得到' },
  { product: 'minimax', scope: 'account', parent: 'minimax', account: 'acct-B',
    supported: true, auto: false, activity_on: true, signed_today: null, claimable: false,
    shared: true, unit: '积分',
    note: '这个产品的登录态是整机一份，换号会覆盖前一个；签到状态要在切到该号之后才读得到' },
  { product: 'qoder-cn', supported: true, auto: false, activity_on: false, signed_today: false,
    claimable: false, streak: 0, reward: 100, unit: '积分', campaign: 'cn_daily_check_in_legacy' },
  // 账号级子行（`scope:"account"` + `parent`）：只有"每个号都留着自己那份登录态"的
  // 产品才有——现在 Qoder 系与 WorkBuddy 系。后端在 refresh 里按账号各查一次，
  // 前端把 scope!=="account" 的当产品行，其余挂到它 parent 底下。
  { product: 'qoder-cn', scope: 'account', parent: 'qoder-cn', account: 'shen1950',
    supported: true, auto: false, activity_on: true, signed_today: true, claimable: false,
    streak: 3, reward: 100, unit: '积分', ends_on: '2026-11-15 11:23' },
  { product: 'qoder-cn', scope: 'account', parent: 'qoder-cn', account: 'nick9994374911',
    supported: true, auto: true, activity_on: true, signed_today: false, claimable: true,
    streak: 5, reward: 100, unit: '积分', ends_on: '2026-11-14 11:32' },
  { product: 'qoder', supported: true, auto: false, activity_on: false, signed_today: null,
    claimable: false, unit: '积分',
    note: '国际版只有活动开关，领取在网页里点，程序不代点' },
];
/* 后端现在把显示名与本机时间一起带回来（提醒文案、卡片角标都读它），mock 照同一形状 */
const CI_LABEL = {
  workbuddy: 'WorkBuddy', 'workbuddy-intl': 'WorkBuddy 国际版', 'trae-cn': 'Trae CN',
  minimax: 'MiniMax Code', 'qoder-cn': 'Qoder CN', qoder: 'Qoder', astudio: 'AStudio',
  'workbuddy-activity': 'WorkBuddy 活跃地图', 'workbuddy-travel': 'WorkBuddy 旅行猫',
};
// store 名 → app id（真机上 Trae 两边叫 trae-cn / traework-cn，这里照同一张表写）
const CI_APP = {
  workbuddy: 'workbuddy', 'workbuddy-intl': 'workbuddy-intl',
  'workbuddy-activity': 'workbuddy', 'workbuddy-travel': 'workbuddy',
  'trae-cn': 'traework-cn', minimax: 'minimax', astudio: 'astudio',
  'qoder-cn': 'qoder-cn', qoder: 'qoder',
};
CI_ITEMS.forEach((x) => {
  // 账号行的标题是账号名（后端 refresh 里就是这么改的），别被产品名盖掉
  x.app = CI_APP[x.product] || x.product;
  x.name = x.scope === 'account' ? x.account : (CI_LABEL[x.product] || x.product);
  x.today = new Date().toISOString().slice(0, 10);
  x.hour = new Date().getHours();
  x.minute = new Date().getMinutes();
});
DATA.checkin = {
  ok: true, items: CI_ITEMS, fetched_at: Date.now() / 1000,
  state: { done: { workbuddy: CI_ITEMS[0].today }, reminded: '',
    // 正在等到账 + 已到账的提示：首页与签到页都要看这两段
    pending: { 'workbuddy-intl': { since: Date.now() / 1000 - 3600,
      today: CI_ITEMS[0].today, label: 'WorkBuddy 国际版', expect: '今天这 30 积分',
      until: Date.now() / 1000 + 86400 } },
    arrived: [],
    log: [{ at: CI_ITEMS[0].today, store: 'workbuddy', claimed: true, credit: 100, message: '已领到' }] },
};
// 「一键刷新全部额度」的后台任务：mock 里先给一个跑完的，进度条与失败清单有得看
DATA.alljob = {
  running: false, done: 9, total: 9, current: '', ok: 8, fail: 1,
  started_at: Date.now() / 1000 - 120, finished_at: Date.now() / 1000 - 90,
  log: [{ app: 'Qoder CN', name: 'nick9994374911', ok: false,
    msg: '这个号的官网会话还没登录：先在驾驶舱打开的官网窗口里登录一次' }],
};
/* 真后端是按 app id 去缓存里查这家再挂到实例上（state.rs 的 checkin::for_app），
   mock 照同一份映射挂，卡片角标才有得显示 */
const APP_STORE = {
  workbuddy: 'workbuddy', 'workbuddy-intl': 'workbuddy-intl', 'traework-cn': 'trae-cn',
  minimax: 'minimax', 'qoder-cn': 'qoder-cn', qoder: 'qoder',
};
DATA.instances.forEach((i) => {
  const it = CI_ITEMS.find((x) => x.product === APP_STORE[i.app]);
  if (!it) return;
  i.checkin = {
    state: it.signed_today ? 'done' : it.claimable ? 'claimable' : it.activity_on === false ? 'off' : 'unknown',
    reward: it.reward, streak: it.streak, auto: it.auto, name: it.name,
  };
});
DATA.app_version = '2.0.0';

DATA.backups = [
  { file: 'workbuddy__DuckDeity__20261002-160701.zip', app: 'workbuddy', account: 'DuckDeity', bytes: 184549376, mb: 176.0, at: '2026-10-02 16:07' },
  { file: 'traework-cn__账号1__20261002-155822.zip', app: 'traework-cn', account: '账号1', bytes: 21495808, mb: 20.5, at: '2026-10-02 15:58' },
];

window.__TAURI__ = {
  core: {
    invoke(cmd, args = {}) {
      if (cmd === 'get_state') return Promise.resolve(JSON.parse(JSON.stringify(DATA)));
      if (cmd === 'get_settings') return Promise.resolve(JSON.parse(JSON.stringify(DATA.settings)));
      if (cmd === 'set_settings') {
        const p = args.patch || {};
        for (const [sec, vals] of Object.entries(p)) {
          DATA.settings[sec] = { ...(DATA.settings[sec] || {}), ...vals };
        }
        return Promise.resolve(JSON.parse(JSON.stringify(DATA.settings)));
      }
      if (cmd === 'set_autostart') {
        DATA.settings.general.autostart = args.enable;
        return Promise.resolve({ ok: true, msg: args.enable ? '已开启开机自启' : '已关闭开机自启' });
      }
      if (cmd === 'scan_inventory') {
        return Promise.resolve({
          ok: true, detected: 12, first_run: false,
          new: ['astudio'], gone: ['qodercnide'],
          catalog: { loaded: 12, path: 'C:/Users/x/.agents/catalog.json' },
          stale_paths: [
            { id: 'qoder', name: 'Qoder', recorded: ['{LOCALAPPDATA}/Programs/Qoder/Qoder.exe'],
              found_at: ['D:/Artificial_Intelligence/Qoder/Qoder.exe'], has_profile: true },
            { id: 'qoder-cn-ide', name: 'Qoder CN IDE', recorded: ['D:/Artificial_Intelligence/Qoder CN IDE/Qoder CN IDE.exe'],
              found_at: [], has_profile: false },
          ],
          detail: {},
        });
      }
      if (cmd === 'get_key_plain') {
        for (const p of DATA.keys) {
          const k = (p.keys || []).find((x) => x.id === args.id);
          if (k) return Promise.resolve(k.key || '');
        }
        return Promise.resolve('');
      }
      if (cmd === 'get_meta') {
        return Promise.resolve({ token: 'sk-demo-xxxxxxxxxxxx', quota_url: '', quota_path: '', quota_headers: '{}', quota_adapter: 'openai' });
      }
      if (cmd === 'add_app') {
        const exe = String(args.exe || '');
        const base = exe.split(/[\\/]/).pop() || '';
        const name = args.name || base.replace(/\.[^.]+$/, '');
        if (!base) return Promise.resolve({ ok: false, msg: `找不到这个文件：${exe}` });
        const id = `custom-${name.toLowerCase().replace(/\s/g, '')}`;
        if (!DATA.apps.some((a) => a.id === id)) {
          DATA.apps.push({ id, name, kind: 'electron', installed: true, swapped: '', swap_ok: true, custom: true });
        }
        return Promise.resolve({ ok: true, msg: `已加入「${name}」——识别为 Chromium 内核，可以多开隔离`, app: id });
      }
      if (cmd === 'remove_app') {
        if (!String(args.app).startsWith('custom-')) {
          return Promise.resolve({ ok: false, msg: '只有手动添加的程序能移除，自动发现的请用右键「隐藏此程序」' });
        }
        DATA.apps = DATA.apps.filter((a) => a.id !== args.app);
        return Promise.resolve({ ok: true, msg: '已从列表移除，账号目录原样保留' });
      }
      if (cmd === 'action' && (args.payload || {}).action === 'refresh_all') {
        // 真后端是起后台任务 + 4 秒轮询进度；mock 里直接给一份"刚跑完"的，形状保持一致
        DATA.alljob = {
          running: false, done: DATA.instances.length, total: DATA.instances.length,
          current: '', ok: DATA.instances.length, fail: 0,
          started_at: Date.now() / 1000 - 40, finished_at: Date.now() / 1000,
          log: [],
        };
        return Promise.resolve({ ok: true, started: true, msg: '已经开始逐个读各账号的额度（演示数据）' });
      }
      if (cmd === 'action' && /^checkin_(view|refresh)$/.test((args.payload || {}).action || '')) {
        const today = new Date().toISOString().slice(0, 10);
        const items = JSON.parse(JSON.stringify(CI_ITEMS)).map((x) => {
          x.today = today; x.hour = new Date().getHours(); return x;
        });
        return Promise.resolve({
          ok: true, items, fetched_at: Date.now() / 1000,
          state: { done: { workbuddy: today },
            pending: DATA.checkin.state.pending, arrived: DATA.checkin.state.arrived,
            log: [{ at: today, store: 'workbuddy', claimed: true, credit: 100, message: '已领到' }] },
        });
      }
      if (cmd === 'action' && (args.payload || {}).action === 'checkin_claim') {
        return Promise.resolve({ ok: true, result: { claimed: true, already: false,
          credit: 100, message: '已领到（演示数据）', today: new Date().toISOString().slice(0, 10) } });
      }
      if (cmd === 'action' && (args.payload || {}).action === 'checkin_auto') {
        const p = args.payload || {};
        DATA.settings.checkin = { ...DATA.settings.checkin,
          ...(p.enabled === undefined ? {} : { enabled: p.enabled }),
          ...(p.auto ? { auto: p.auto } : {}),
          ...(p.window_start === undefined ? {} : { window_start: p.window_start }),
          ...(p.window_end === undefined ? {} : { window_end: p.window_end }) };
        return Promise.resolve({ ok: true, msg: '已保存（演示数据）' });
      }
      if (cmd === 'action' && args.payload?.action === 'auth_switch') {
        const p = args.payload;
        const a = DATA.apps.find((x) => x.id === p.app) || {};
        const i = DATA.instances.find((x) => x.app === p.app && x.name === p.name);
        if (!i || !i.auth_pack) {
          return Promise.resolve({ ok: false, msg: `「${p.name}」还没存过登录态，切不过去。` });
        }
        const cur = DATA.instances.find((x) => x.app === p.app && x.logged_in);
        if (cur) { cur.logged_in = false; cur.running = false; cur.procs = 0; }
        i.logged_in = true;
        if (i.auth_pack) i.auth_pack.at = Date.now() / 1000;
        const mv = ((a.machine || {}).value) || {};
        mv.account = i.auth_pack.account; mv.user_id = i.auth_pack.user_id;
        return Promise.resolve({
          ok: true,
          msg: `已切到「${p.name}」：收下了「${cur ? cur.name : '当前号'}」的登录态 → 结束客户端（8 个进程）`
            + ` → 写回目标登录态 → 重新打开。\n原来那份备份在 ~/.agent-cockpit/backups/auth/${p.app}-1760000000，随时能切回来。`,
        });
      }
      if (cmd === 'action' && args.payload?.action === 'swap_set') {
        const p = args.payload;
        const a = DATA.apps.find((x) => x.id === p.app) || {};
        // 共用一份档案：后端把"这次什么都没换"如实说回来（noop），界面不许再去关程序
        if (a.shared_profile) {
          return Promise.resolve({
            ok: true, noop: true, need_force: false,
            msg: `${a.name} 的所有账号共用这一份档案，切到「${p.name}」在磁盘上没有东西要换。\n`
              + '真正的切换在客户端里做：退出登录，再登这个号。',
          });
        }
        if (p.force) return Promise.resolve({ ok: true, msg: `已换号：${a.name} → 「${p.name}」（演示）` });
        // 真被进程挡住时后端给结构化 need_force，前端据此摆「结束进程并换号」
        return Promise.resolve({
          ok: false, need_force: true,
          msg: 'C:\\Users\\x\\AppData\\Roaming\\com.qoder.app.stable 现在被这些进程占着：qoder.exe（PID 4108）。\n'
            + '它们没有窗口但在后台驻留（换号、改名都会被挡住）。确认后驾驶舱可以替你结束这些进程再换号。',
        });
      }
      if (cmd === 'action' && args.payload?.action === 'key_probe_models') {
        return Promise.resolve({ ok: true, msg: '拉到 7 个模型（演示数据）',
          models: ['gpt-5.5', 'gpt-5.5-mini', 'claude-opus-4-6', 'claude-sonnet-4-6',
            'deepseek-v4', 'glm-5', 'kimi-k3'] });
      }
      // 站点内批量管理（勾选 / 全选 / 改属性 / 移动 / 删除）——演示数据下也要能点通，
      // 否则前端那条路一改就没人验证得到（2026-10-17 加的）
      if (cmd === 'action' && args.payload?.action === 'key_dedup_scan') {
        const site = (DATA.keys || [])[0] || { id: 'p-longcat', name: 'LongCat' };
        const ks = (site.keys || []).slice(0, 4);
        const groups = ks.length >= 3 ? [{
          masked: ks[0].masked || 'sk-ws••••nBzS',
          site_name: site.name, count: ks.length,
          entries: ks.map((k, i) => ({
            site: site.id, site_name: site.name, id: k.id, label: k.label || `用途${i + 1}`,
            masked: k.masked, days_left: k.days_left, last_status: k.last_status,
            use_in_proxy: k.use_in_proxy,
          })),
        }] : [];
        return Promise.resolve({ ok: true, locked: false, groups });
      }
      if (cmd === 'action' && (args.payload?.action === 'key_batch_update'
        || args.payload?.action === 'key_batch_move'
        || args.payload?.action === 'key_delete_batch'
        || args.payload?.action === 'key_test')) {
        const p = args.payload || {};
        const n = (p.ids || []).length || 1;
        return Promise.resolve({
          ok: true,
          msg: p.action === 'key_batch_move' ? `已挪过去 ${n} 条（演示数据）`
            : p.action === 'key_batch_update' ? `已改 ${n} 条（演示数据）`
              : p.action === 'key_delete_batch' ? `已删除 ${n} 条（演示数据）`
                : '可用（演示数据）',
        });
      }
      if (cmd === 'action' && args.payload?.action === 'key_upsert') {
        // 真后端会回最终站点 id（同名/同站族会并站），界面靠它接着往下走
        return Promise.resolve({ ok: true, msg: '已保存站点与 key（演示数据）',
          site: 'p-demo' });
      }
      if (cmd === 'action' && args.payload?.action === 'seed_sources') {
        // 向导「数据来源」那一步：有哪些档案、分别是谁在用（只给路径和体积人认不出来）
        const id = args.payload.app;
        return Promise.resolve({
          ok: true, recommend: id === 'zcode' ? 'adopt' : 'copy',
          default_claimed_by: id === 'qoder-cn' ? 'nick9994374911' : '',
          recommend_from: id === 'zcode'
            ? 'C:\\Users\\LENOVO\\.zcode.cockpit-original'
            : 'C:\\Users\\LENOVO\\.agent-cockpit\\instances\\zcode\\账号1',
          sources: [
            { origin: 'adopt', kind: 'profile', title: '你一直在用的那份', label: '本机的原有档案',
              path: 'C:\\Users\\LENOVO\\.zcode.cockpit-original',
              junction: false, has_data: true, mb: 268,
              facts: { who: 'renlu132', last_used: '2026-10-11 13:22', files: 1842, is_default: true, claimed_by: id === 'qoder-cn' ? 'nick9994374911' : '' },
              note: '程序现在打开的就是这一份' },
            { origin: 'copy', kind: 'instance', title: '账号「账号1」', label: '已有账号「账号1」', name: '账号1',
              path: 'C:\\Users\\LENOVO\\.agent-cockpit\\instances\\zcode\\账号1',
              junction: false, has_data: true, mb: 12,
              facts: { who: '', last_used: '2026-10-06 21:04', files: 96, is_default: false, claimed_by: '账号1' },
              note: '复制一份过来：对话与记忆都带过来，它本身不动' },
          ],
        });
      }
      if (cmd === 'action' && args.payload?.action === 'machine_login') {
        return Promise.resolve({
          ok: true, account: 'shen1950', user_id: '019e8247-2e21-7aa6-9e87-b0720003b387',
          user_type: 'personal_standard', left: 1751, total: 2400, expires_on: '未给出',
          source: 'server', stale: false, live_error: '',
          auth_from: 'C:\\Users\\LENOVO\\AppData\\Roaming\\com.qodercn.app.stable\\auth.v1.dat',
          portions: [
            { label: '主额度', left: 0, total: 0, expires_on: '无限期', category: '套餐基础积分' },
            { label: '平台奖励积分·平台赠送', left: 1751, total: 2400, expires_on: '2026-10-24 21:13', category: '平台奖励积分' },
          ],
        });
      }
      if (cmd === 'action' && args.payload?.action === 'quota_refresh') {
        // 后端这一条自己挑层：官网逐包（唯一给得出每包各自到期的）→ 读不到才退服务端/本机。
        // 演两条路：qoder-cn 有这个号的官网会话，其它产品还没登过——要看的就是那句"缺了哪一层"
        if (args.payload.app === 'qoder-cn') {
          return Promise.resolve({
            ok: true, source: 'web',
            msg: '官网读数已刷新：剩 1751 / 共 2400 积分，已用 649 · 逐包 4 笔（最早 2026-10-22 09:30 作废）\n'
              + '套餐：体验版 · 每日消耗 371 天',
          });
        }
        return Promise.resolve({
          ok: true, source: 'server', need_login: true, detail_error: '这个号的官网会话还没登录',
          msg: '读到「DuckDeity」：剩 4194.84 / 4943 credits，到期 未给出\n\n'
            + '这一轮的逐包额度和各自的到期时间没读到：这个号的官网会话还没登录\n'
            + '点「去登录」，在弹出的窗口里登这个号的阿里云账号（只登一次），之后每次刷新都直接补齐。',
        });
      }
      if (cmd === 'action' && args.payload?.action === 'web_fetch') {
        // 2026-10-11 真从 qoder.com.cn/account/usage 抓回来的形状（截两笔）
        return Promise.resolve({
          ok: true,
          data: {
            ok: true, user_id: '019e8247-2e21-7aa6-9e87-b0720003b387', account: 'shen1950',
            plan: 'PLAN_TIER_FREE', plan_name: '体验版', plan_expire: '无限期',
            left: 1751, total: 2400, used: 649, quota_key: 'big_model_credits',
            page: 'https://qoder.com.cn/account/usage',
            groups: [
              { name: '套餐基础积分', unit: 'credits', left: 0, total: 0, used: 0, n: 1,
                items: [{ label: '套餐额度', pool: '套餐基础积分', detail: 'PLAN', left: 0, total: 0, used: 0, expires_on: '未给出', unit: 'credits', status: 'ACTIVE' }] },
              { name: '平台奖励积分', unit: 'credits', left: 1751, total: 2200, used: 449, n: 2,
                items: [
                  { label: '平台赠送', pool: '平台奖励积分', detail: 'RESOURCE_PACKAGE_SOURCE_BONUS', left: 51, total: 100, used: 49, expires_on: '2026-10-24 21:13', unit: 'credits', status: 'ACTIVE' },
                  { label: '平台赠送', pool: '平台奖励积分', detail: 'RESOURCE_PACKAGE_SOURCE_BONUS', left: 100, total: 100, used: 0, expires_on: '2026-11-10 13:13', unit: 'credits', status: 'ACTIVE' },
                ] },
              { name: '购买积分', unit: 'credits', left: 0, total: 200, used: 200, n: 1,
                items: [{ label: '购买', pool: '购买积分', detail: 'RESOURCE_PACKAGE_SOURCE_PURCHASE', left: 0, total: 200, used: 200, expires_on: '2026-10-22 09:30', unit: 'credits', status: 'ACTIVE' }] },
            ],
            portions: [
              { label: '套餐基础积分·套餐额度', left: 0, total: 0, used: 0, expires_on: '未给出', category: '套餐基础积分', unit: 'credits' },
              { label: '平台奖励积分·平台赠送', left: 51, total: 100, used: 49, expires_on: '2026-10-24 21:13', category: '平台奖励积分', unit: 'credits' },
              { label: '平台奖励积分·平台赠送', left: 100, total: 100, used: 0, expires_on: '2026-11-10 13:13', category: '平台奖励积分', unit: 'credits' },
              { label: '购买积分·购买', left: 0, total: 200, used: 200, expires_on: '2026-10-22 09:30', category: '购买积分', unit: 'credits' },
            ],
          },
        });
      }
      if (cmd === 'action' && args.payload?.action === 'web_login') {
        return Promise.resolve({ ok: true, msg: '已打开官网登录窗口（演示数据）' });
      }
      if (cmd === 'action' && args.payload?.action === 'new_instance') {
        return Promise.resolve({ ok: true, origin: args.payload.origin || 'auto',
          msg: `已新建 ${args.payload.app}「${args.payload.name}」。演示数据` });
      }
      const msg = `${args.payload?.action ?? cmd} → ${args.payload?.name ?? args.payload?.app ?? ''}`;
      return Promise.resolve({ ok: true, msg });
    },
  },
  window: {
    getCurrentWindow() {
      return {
        setTitle(t) { document.title = t; return Promise.resolve(); },
        hide() { return Promise.resolve(); },
        show() { return Promise.resolve(); },
        minimize() { return Promise.resolve(); },
        toggleMaximize() { return Promise.resolve(); },
        isMaximized() { return Promise.resolve(false); },
        onResized() { return Promise.resolve(() => {}); },
        setFocus() { return Promise.resolve(); },
        onCloseRequested() { return Promise.resolve(() => {}); },
      };
    },
  },
};
