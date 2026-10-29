/* Agent 驾驶舱 · 手机远程端逻辑。
 * 与桌面 app.js 同一套 action 协议，只是桥从 Tauri invoke 换成 fetch：
 *   GET  /api/state          → 桌面 get_state 的出网投影（remote.rs curate）
 *   POST /api/action         → 桌面 action 的白名单子集
 *   POST /api/pair {code}    → 一次性配对码换 token
 * token 只存 localStorage；401 时弹配对门。 */
(() => {
  'use strict';
  const $ = (id) => document.getElementById(id);
  const TOKEN_KEY = 'acRemoteToken';
  let S = null;              // 最近一次 /api/state
  let timer = 0;
  let busy = false;
  let pendingPairCode = null; // 仅在当前页面内存中保留，以便网络失败后重试
  let pairing = false;
  let pollingStarted = false;
  let visibilityListenerInstalled = false;

  /* ---------- 小工具 ---------- */
  const el = (tag, attrs, ...kids) => {
    const n = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs || {})) {
      if (k === 'class') n.className = v;
      else if (k.startsWith('on')) n.addEventListener(k.slice(2), v);
      else n.setAttribute(k, v);
    }
    for (const k of kids.flat()) {
      if (k != null) n.append(k);
    }
    return n;
  };
  const txt = (s) => document.createTextNode(String(s ?? ''));
  const fmtNum = (v) => (typeof v === 'number' ? v.toLocaleString('zh-CN', { maximumFractionDigits: 1 }) : '–');
  const ago = (ts) => {
    if (!ts) return '还没读过';
    const s = Math.max(0, Date.now() / 1000 - ts);
    if (s < 90) return '刚刚';
    if (s < 5400) return Math.round(s / 60) + ' 分钟前';
    if (s < 172800) return Math.round(s / 3600) + ' 小时前';
    return Math.round(s / 86400) + ' 天前';
  };
  let toastT = 0;
  const toast = (m) => {
    const t = $('toast');
    t.textContent = m;
    t.classList.add('on');
    clearTimeout(toastT);
    toastT = setTimeout(() => t.classList.remove('on'), 2600);
  };
  const pill = (cls, label) => el('span', { class: 'pill ' + cls }, txt(label));

  /* ---------- API ---------- */
  async function api(path, body) {
    const requestToken = localStorage.getItem(TOKEN_KEY);
    const r = await fetch(path, {
      method: body ? 'POST' : 'GET',
      headers: {
        ...(body ? { 'Content-Type': 'application/json' } : {}),
        Authorization: 'Bearer ' + requestToken,
      },
      body: body ? JSON.stringify(body) : undefined,
    });
    if (r.status === 401 || r.status === 429) {
      // 不让较早请求的 401 清除刚刚重新配对得到的新 token。
      if (localStorage.getItem(TOKEN_KEY) === requestToken) {
        localStorage.removeItem(TOKEN_KEY);
        stopPolling();
        showGate(r.status === 429 ? '设备凭据未通过验证，请重新扫码或输入配对码。' : '配对已失效（可能在桌面端被吊销），重新扫码');
      }
      throw new Error('401');
    }
    const v = await r.json().catch(() => ({}));
    if (!r.ok) throw new Error(v.msg || ('HTTP ' + r.status));
    return v;
  }

  /* ---------- 配对 ---------- */
  function showGate(msg) {
    if (msg) $('gateErr').textContent = msg;
    $('gate').classList.add('on');
  }
  function hideGate() { $('gate').classList.remove('on'); }

  function deviceName() {
    const ua = navigator.userAgent;
    if (/iPhone/.test(ua)) return 'iPhone';
    if (/iPad/.test(ua)) return 'iPad';
    if (/Android/.test(ua)) return 'Android ' + (ua.match(/Android ([\d.]+)/) || [])[1];
    return '浏览器';
  }

  async function pairWithCode(input) {
    if (pairing) return false;
    const code = String(input || '').trim().toUpperCase();
    if (!/^[0-9A-F]{32}$/.test(code)) {
      showGate('请输入电脑上显示的 32 位配对码，包含数字和字母 A 到 F。');
      return false;
    }
    pendingPairCode = code;
    pairing = true;
    $('gatePair').disabled = true;
    $('gateRetry').disabled = true;
    try {
      const r = await fetch('/api/pair', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ code, name: deviceName() }),
      });
      const v = await r.json().catch(() => ({}));
      if (!r.ok || !v.ok || typeof v.token !== 'string' || !v.token) {
        if (r.status === 401) pendingPairCode = null;
        showGate(r.status === 429 ? '尝试次数过多，请稍后重试。' : (v.msg || '配对未完成，请检查配对码是否有效。'));
        return false;
      }
      localStorage.setItem(TOKEN_KEY, v.token);
      pendingPairCode = null;
      $('gateCode').value = '';
      $('meName').textContent = v.name || deviceName();
      hideGate();
      toast('配对成功');
      refresh(true);
      startPolling();
      return true;
    } catch (_) {
      showGate('连接中断，请点击“重试”。如果配对码已失效，请在电脑上重新生成。');
      return false;
    } finally {
      pairing = false;
      $('gatePair').disabled = false;
      $('gateRetry').disabled = false;
    }
  }

  async function tryPairFromHash() {
    const m = location.hash.match(/[#&]t=([A-Za-z0-9]+)/);
    if (!m) return false;
    const code = m[1];
    history.replaceState(null, '', location.pathname); // 先从地址栏和历史记录移除配对码
    return pairWithCode(code);
  }

  /* ---------- 渲染 ---------- */
  function setConn(ok, text) {
    $('connDot').classList.toggle('off', !ok);
    $('connText').textContent = text;
  }

  function renderDash() {
    // 一键刷新进度（alljob）
    const j = S.alljob;
    const showJob = j && j.running;
    $('refreshCard').hidden = !showJob;
    if (showJob) {
      $('jobText').textContent = '正在读 ' + (j.current || '…');
      $('jobSub').textContent = '成功 ' + (j.ok || 0) + ' · 失败 ' + (j.fail || 0);
      $('jobNum').textContent = (j.done || 0) + '/' + (j.total || 0);
      $('jobFill').style.width = (j.total ? Math.round((j.done || 0) / j.total * 100) : 0) + '%';
    }
    // 临期徽章与列表
    const alerts = (S.alerts || []);
    const danger = alerts.filter((a) => (a.days ?? 99) <= 7);
    const badge = $('alertBadge');
    if (danger.length) { badge.hidden = false; badge.className = 'badge'; badge.textContent = danger.length + ' 项 7 天内到期'; }
    else if (alerts.length) { badge.hidden = false; badge.className = 'badge ok'; badge.textContent = alerts.length + ' 项临期'; }
    else badge.hidden = true;
    $('alertCard').hidden = !alerts.length;
    const list = $('alertList');
    list.textContent = '';
    for (const a of alerts.slice(0, 8)) {
      list.append(el('div', { class: 'row' },
        el('div', { class: 'grow' },
          el('div', { class: 't' }, txt(`${a.app} · ${a.name}`)),
          el('div', { class: 'd' }, txt(`${a.expires ? a.expires + ' 到期 · ' : ''}剩 ${fmtNum(a.left)} ${a.unit || '积分'}`))),
        pill((a.days ?? 99) <= 7 ? 'bad' : 'warn', a.days != null ? `剩 ${a.days} 天` : '临期')));
    }
    // 概况
    $('statRun').textContent = S.stats?.running ?? '–';
    $('statSub').textContent = `共 ${S.stats?.instances ?? 0} 个账号档案 · ${S.stats?.agents ?? 0} 个 Agent`;
    const pend = pendingCheckins();
    $('statCheckin').textContent = pend.length || '0';
    $('checkinSub').textContent = pend.length ? pend.slice(0, 3).map((x) => x.name || x.product).join('、') : '今天都领过了';
    $('proxyPill').textContent = S.proxy?.running ? '运行中' : '未开';
    $('proxyPill').className = 'pill ' + (S.proxy?.running ? 'ok' : 'grey');
    $('proxySub').textContent = S.proxy?.running ? `接入点 ${S.proxy.base_url || ''}` : '在桌面端「本地反代」里开';
  }

  function pendingCheckins() {
    const items = (S.checkin && S.checkin.items) || [];
    return items.filter((x) => x.supported !== false && !x.claimed && (x.claimable || x.pending));
  }

  /* 逐笔额度包（官网那些各自到期的赠送包）展开状态：key = app/账号名 */
  const openInst = new Set();

  function portionRow(c) {
    const dl = c.days_left;
    return el('div', { class: 'row' },
      el('div', { class: 'grow' },
        el('div', { class: 't' }, txt(c.label || '额度包')),
        el('div', { class: 'd' }, txt(`${fmtNum(c.left)} / ${fmtNum(c.total)} ${c.unit || ''}${c.expires_on ? ' · ' + c.expires_on + ' 过期' : ''}`))),
      dl != null ? pill(dl <= 7 ? 'bad' : dl <= 30 ? 'warn' : 'grey', `剩 ${dl} 天`) : null);
  }

  function renderQuota() {
    const list = $('quotaList');
    list.textContent = '';
    const inst = S.instances || [];
    if (!inst.length) { list.append(el('div', { class: 'card empty' }, txt('还没有账号档案，先在桌面端建卡'))); }
    let lastApp = '';
    for (const i of inst) {
      if (i.app_name !== lastApp) {
        lastApp = i.app_name;
        list.append(el('div', { class: 'meta', style: 'margin:4px 2px 6px' }, txt(lastApp)));
      }
      const q = i.quota || {};
      const has = q.left != null || q.total != null;
      const pct = q.total > 0 && q.left != null ? Math.round(q.left / q.total * 100) : null;
      const chip = q.days_left != null
        ? pill(q.days_left <= 7 ? 'bad' : q.days_left <= 30 ? 'warn' : 'grey', `剩 ${q.days_left} 天`)
        : (i.running ? pill('ok', '在跑') : pill('grey', '没在跑'));
      const src = { web: '官网', local: '本机', adapter: '适配器' }[q.source] || q.source || '';
      // 还有余额的逐笔包才有展开的意义；全过期的收起来省地方
      const credits = (q.credits || []).filter((c) => (c.left ?? 0) > 0);
      const key = i.app + '/' + i.name;
      const isOpen = openInst.has(key);
      const card = el('div', { class: 'card' },
        el('div', { class: 'row', style: 'border:none;padding-top:0' },
          el('div', { class: 'grow' },
            el('div', { class: 't' }, txt(i.name)),
            el('div', { class: 'd' }, txt(`${src ? src + ' · ' : ''}${ago(q.fetched_at)}`))),
          chip),
        has
          ? el('div', { class: 'kv' },
              txt('剩余 / 总量'),
              el('span', { class: 'val' }, txt(`${fmtNum(q.left)} / ${fmtNum(q.total)} ${q.unit || ''}`)))
          : el('div', { class: 'meta', style: 'margin-top:6px' }, txt(q.live_error || q.web_error || '还没有额度读数')),
        pct != null ? el('div', { class: 'track' }, el('div', { class: 'fill' + (q.days_left != null && q.days_left <= 7 ? ' warn' : ''), style: 'width:' + pct + '%' })) : null);
      if (credits.length) {
        card.append(el('div', { style: 'margin-top:9px' },
          el('button', { class: 'btn ghost sm', onclick: () => {
            if (isOpen) openInst.delete(key); else openInst.add(key);
            renderQuota();
          } }, txt(isOpen ? '收起逐笔' : `逐笔 ${credits.length} 包 · 展开看到期`)),
          isOpen ? el('div', { style: 'margin-top:2px' }, credits.slice(0, 40).map(portionRow)) : null));
      }
      list.append(card);
    }
    // 站点 Key 台账：全量列出（不只是临期的），每把可测活
    const sl = $('siteList');
    sl.textContent = '';
    const sites = S.keys || [];
    const siteRows = [];
    for (const site of sites) {
      const keys = site.keys || [];
      if (!keys.length) continue;
      siteRows.push(el('div', { class: 'meta', style: 'margin:4px 2px 2px' }, txt(`${site.name} · ${keys.length} 把`)));
      for (const k of keys) {
        const dl = k.days_left;
        siteRows.push(el('div', { class: 'row' },
          el('div', { class: 'grow' },
            el('div', { class: 't' }, txt(k.masked || k.id || 'key')),
            el('div', { class: 'd' }, txt(`${k.label ? k.label + ' · ' : ''}${k.left != null ? fmtNum(k.left) + (k.total != null ? ' / ' + fmtNum(k.total) : '') : '额度未知'}${k.expires_on ? ' · ' + k.expires_on : ''}`))),
          dl != null ? pill(dl <= 0 ? 'bad' : dl <= 30 ? 'warn' : 'grey', dl <= 0 ? '已过期' : `剩 ${dl} 天`) : null,
          el('button', { class: 'btn ghost sm', onclick: async (ev) => {
            const b = ev.currentTarget;
            b.disabled = true;
            b.textContent = '测中…';
            try {
              const r = await api('/api/action', { action: 'key_test', site: site.id, id: k.id });
              toast(r.ok ? (r.msg || '这把 key 是活的') : (r.msg || '测活失败'));
            } catch (e) { toast(e.message); }
            b.disabled = false;
            b.textContent = '测活';
          } }, txt('测活'))));
      }
    }
    $('siteCard').hidden = !siteRows.length;
    sl.append(...siteRows);
  }

  function renderCheckin() {
    const items = (S.checkin && S.checkin.items) || [];
    $('checkinMeta').textContent = S.checkin?.stale ? '读的是缓存，正在后台刷新' : '更新于 ' + ago(S.checkin?.fetched_at);
    const list = $('checkinList');
    list.textContent = '';
    if (!items.length) { list.append(el('div', { class: 'empty' }, txt('还没有签到数据'))); }
    for (const x of items) {
      if (x.supported === false) continue;
      const state = x.claimed ? 'ok'
        : x.claimable ? 'blue'
        : 'grey';
      const label = x.claimed ? `已领${x.credit != null ? ' +' + x.credit : ''}`
        : x.claimable ? '可领取'
        : (x.pending ? '待查' : '今日无可领');
      // 右侧操作排：自动领开关（写了设置，回桌面也能看到同一份）+ 状态 + 领取
      const right = el('div', { style: 'display:flex;gap:5px;align-items:center;flex-wrap:wrap;justify-content:flex-end' },
        el('button', {
          class: 'btn ghost sm', title: '到时间窗自动领',
          onclick: async (ev) => {
            const b = ev.currentTarget;
            b.disabled = true;
            try {
              await api('/api/action', { action: 'checkin_auto', auto: { [x.product]: !x.auto } });
              toast(x.auto ? '已关：这家不再自动领' : '已开：到时间窗自动领');
              setTimeout(() => refresh(true), 600);
            } catch (e) { toast(e.message); b.disabled = false; }
          },
        }, txt(x.auto ? '自动·开' : '自动·关')),
        pill(state, label));
      if (x.claimable && !x.claimed) {
        right.append(el('button', { class: 'btn sm', onclick: (ev) => claim(x, ev.currentTarget) }, txt('领取')));
      }
      list.append(el('div', { class: 'row' },
        el('div', { class: 'grow' },
          el('div', { class: 't' }, txt(x.name || x.product)),
          el('div', { class: 'd' }, txt(x.reward != null ? `每日 ${x.reward} ${x.unit || '积分'}` : (x.account || '')))),
        right));
    }
    renderCiLog();
  }

  /* 领取日志（checkin.state.log，后端留最近 2000 条）：只读投影，直接挑安全字段 */
  const CI_RANGES = [['today', '今天'], ['d7', '近 7 天']];
  let ciRange = 'today';

  function ciLogTone(x) {
    if (x.claimed === true) return { cls: 'ok', word: `领到${x.credit != null ? ' ' + x.credit : ''}` };
    if (x.settling === true) return { cls: 'blue', word: x.message || '等它到账' };
    if (x.already === true) return { cls: 'grey', word: '今天已领过' };
    return { cls: 'bad', word: `没领成${x.error || x.message ? '：' + (x.error || x.message) : ''}` };
  }

  function renderCiLog() {
    const log = (((S.checkin || {}).state || {}).log) || [];
    const card = $('ciLogCard');
    card.hidden = !log.length;
    if (!log.length) return;
    // 产品名映射：日志里是 store id，签到条目带中文名
    const names = {};
    for (const it of (S.checkin?.items) || []) names[it.product] = it.name;
    const sorted = [...log].sort((a, b) => String(b.at || '').localeCompare(String(a.at || '')));
    const today = new Date().toLocaleDateString('sv');
    const d7 = new Date(Date.now() - 6 * 86400 * 1000).toLocaleDateString('sv');
    const from = ciRange === 'today' ? today : d7;
    const rows = sorted.filter((x) => (x.at || '') >= from).slice(0, 30);
    const ranges = $('ciLogRanges');
    ranges.textContent = '';
    for (const [id, label] of CI_RANGES) {
      ranges.append(el('button', { class: 'btn ghost sm', onclick: () => { ciRange = id; renderCiLog(); } },
        txt(ciRange === id ? `● ${label}` : label)));
    }
    $('ciLogMeta').textContent = rows.length ? `${rows.length} 条` : '这段时间没有记录';
    const host = $('ciLogList');
    host.textContent = '';
    for (const x of rows) {
      const t = ciLogTone(x);
      const who = names[x.store] || x.store || '—';
      host.append(el('div', { class: 'row' },
        el('div', { class: 'grow' },
          el('div', { class: 't' }, txt(`${x.account ? who + ' · ' + x.account : who}`)),
          el('div', { class: 'd' }, txt(x.at || ''))),
        pill(t.cls, t.word)));
    }
  }

  async function claim(x, btn) {
    btn.disabled = true;
    btn.textContent = '领取中…';
    try {
      const r = await api('/api/action', { action: 'checkin_claim', app: '', store: x.product });
      if (r.claimed || r.already) toast(`${x.name || x.product}：已领${r.credit != null ? ' +' + r.credit : ''}`);
      else if (r.settling) toast('已领取，到账自动盯');
      else toast(r.message || r.msg || '没领成，看桌面端详情');
      refresh(true);
    } catch (e) {
      btn.disabled = false;
      btn.textContent = '领取';
      toast('领取失败：' + e.message);
    }
  }

  function renderMore() {
    $('meName').textContent = localStorage.getItem(TOKEN_KEY) ? (deviceName() + ' · ' + location.host) : '未配对';
    $('idleNote').textContent = S.remote?.idle_off_h ? `开启后 ${S.remote.idle_off_h} 小时没人访问就自动关` : '–';
    $('verNote').textContent = '驾驶舱 v' + (S.version || '?') + ' · 远程模式只读+白名单操作';
    // 保管箱：只看状态（锁没锁、几条），内容永远不下来
    const v = S.vault || {};
    $('vaultPill').textContent = v.locked ? '锁定' : '正常';
    $('vaultPill').className = 'pill ' + (v.locked ? 'warn' : 'ok');
    $('vaultSub').textContent = v.supported === false ? '这台电脑不支持 DPAPI'
      : `DPAPI 加密 · ${v.count ?? 0} 条凭据${v.has_password ? ' + 主密码' : ''}`;
    // 反代：运行状态 + 最近几笔走了哪个模型
    $('pxPill2').textContent = S.proxy?.running ? '运行中' : '未开';
    $('pxPill2').className = 'pill ' + (S.proxy?.running ? 'ok' : 'grey');
    $('pxLogSub').textContent = S.proxy?.running ? (S.proxy.base_url || '') : '到桌面端「模型反代」里开';
    const ph = $('pxLogList');
    ph.textContent = '';
    for (const l of (S.proxy?.logs || []).slice(0, 5)) {
      ph.append(el('div', { class: 'row' },
        el('div', { class: 'grow' },
          el('div', { class: 't' }, txt(l.model && l.model !== '-' ? l.model : (l.path || ''))),
          el('div', { class: 'd' }, txt(`${l.ts || ''}${l.account ? ' · ' + l.account : ''}`))),
        pill(l.status && l.status < 400 ? 'ok' : 'bad', l.status ?? '—')));
    }
  }

  function renderAll() {
    if (!S) return;
    renderDash();
    renderQuota();
    renderCheckin();
    renderMore();
  }

  /* ---------- 轮询 ---------- */
  async function refresh(force) {
    if (!localStorage.getItem(TOKEN_KEY)) return;
    if (busy && !force) return;
    busy = true;
    try {
      S = await api('/api/state');
      setConn(true, '已连接');
      hideGate();
      renderAll();
    } catch (e) {
      if (e.message !== '401') setConn(false, '连不上');
    } finally {
      busy = false;
    }
  }

  /* 自适应轮询：一键刷新在跑时 3 秒盯进度（桌面端同一份 alljob），平时 8 秒 */
  function schedule() {
    clearTimeout(timer);
    if (!pollingStarted) return;
    const wait = S && S.alljob && S.alljob.running ? 3000 : 8000;
    timer = setTimeout(async () => {
      if (!document.hidden) await refresh();
      if (pollingStarted) schedule();
    }, wait);
  }
  function startPolling() {
    if (pollingStarted || !localStorage.getItem(TOKEN_KEY)) return;
    pollingStarted = true;
    clearTimeout(timer);
    schedule();
    if (!visibilityListenerInstalled) {
      visibilityListenerInstalled = true;
      document.addEventListener('visibilitychange', () => {
        if (pollingStarted && !document.hidden) { refresh(true); schedule(); }
      });
    }
  }
  function stopPolling() {
    pollingStarted = false;
    clearTimeout(timer);
  }

  /* ---------- Tab 切换 ---------- */
  document.querySelectorAll('nav button').forEach((b) => {
    b.addEventListener('click', () => {
      document.querySelectorAll('nav button').forEach((x) => x.classList.toggle('on', x === b));
      document.querySelectorAll('main section').forEach((s) => s.classList.toggle('on', s.id === 'tab-' + b.dataset.tab));
    });
  });

  /* ---------- 按钮 ---------- */
  $('btnRefreshAll').addEventListener('click', async () => {
    const b = $('btnRefreshAll');
    b.disabled = true;
    try {
      const r = await api('/api/action', { action: 'refresh_all' });
      toast(r.msg || '已开始');
      setTimeout(() => refresh(true), 600);
    } catch (e) { toast(e.message); }
    b.disabled = false;
  });
  $('btnCheckinRefresh').addEventListener('click', async () => {
    const b = $('btnCheckinRefresh');
    b.disabled = true;
    b.textContent = '重查中…';
    try {
      await api('/api/action', { action: 'checkin_refresh' });
      toast('已开始重查');
      setTimeout(() => refresh(true), 1200);
    } catch (e) { toast(e.message); }
    b.disabled = false;
    b.textContent = '↻ 全部重查';
  });
  $('btnUnpair').addEventListener('click', () => {
    localStorage.removeItem(TOKEN_KEY);
    stopPolling();
    pendingPairCode = null;
    $('gateCode').value = '';
    showGate('已清除本机凭据，重新扫码即可');
  });
  $('gateCode').addEventListener('input', () => { pendingPairCode = null; });
  $('gatePairForm').addEventListener('submit', (e) => {
    e.preventDefault();
    pairWithCode($('gateCode').value);
  });
  $('gateRetry').addEventListener('click', () => {
    if (pendingPairCode) pairWithCode(pendingPairCode);
    else if (localStorage.getItem(TOKEN_KEY)) { hideGate(); refresh(true); }
    else showGate('请重新扫码或输入电脑上显示的配对码。');
  });

  /* ---------- 启动 ---------- */
  (async () => {
    if (!localStorage.getItem(TOKEN_KEY)) {
      const paired = await tryPairFromHash();
      if (!paired && !localStorage.getItem(TOKEN_KEY)) { showGate(); return; }
    } else {
      await tryPairFromHash(); // 带 token 也支持扫码重新配对
    }
    await refresh(true);
    startPolling();
  })();
})();
