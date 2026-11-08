/* Live gateway routes. This view only displays events recorded by the gateway. */
globalThis.GatewayRoutes = (() => {
  let visible = false, timer = null, busy = false, generation = 0, data = null;
  let snapshot = null, selected = '', follow = true, error = '', signature = '', expanded = true;
  const active = new Set(['queued', 'waiting', 'attempting', 'connecting', 'running', 'streaming']);
  const labels = { queued:'排队中', waiting:'等待上游', attempting:'正在尝试', connecting:'连接中', running:'处理中',
    streaming:'正在回复', completed:'已完成', success:'已完成', ok:'成功', failed:'失败', error:'失败',
    skipped:'跳过', retrying:'正在改道', cancelled:'已中断' };
  function stateLabel(state) {
    if (typeof state === 'number') return 'HTTP ' + state;
    return labels[state] || (state ? String(state) : '处理中');
  }
  function stateClass(state) {
    if (active.has(state) || state === 'retrying') return 'active';
    if (['failed','error','cancelled'].includes(state)) return 'failed';
    return ['completed','success','ok'].includes(state) ? 'complete' : '';
  }
  function time(value) {
    if (!value) return '—';
    const date = new Date(typeof value === 'number' && value < 1e12 ? value * 1000 : value);
    return Number.isNaN(date.getTime()) ? '—' : date.toLocaleTimeString([], {hour:'2-digit',minute:'2-digit',second:'2-digit'});
  }
  function count(value) { return Number.isFinite(value) ? value.toLocaleString() : '—'; }
  function node(title, subtitle, cls, symbol) {
    return el('div', {class:'gateway-route-node ' + (cls || '')},
      el('span', {class:'gateway-route-symbol', aria:{hidden:'true'}}, symbol),
      el('b', {}, title), el('span', {class:'gateway-route-subtitle'}, subtitle));
  }
  function paint(force = false) {
    const host = $('proxy-routes');
    if (!host || !visible) return;
    const records = Array.isArray(snapshot?.requests) ? snapshot.requests : [];
    if (follow || !records.some(r => String(r.id) === selected)) selected = records.length ? String(records[0].id) : '';
    const current = records.find(r => String(r.id) === selected);
    const next = JSON.stringify([snapshot, selected, follow, error, data?.proxy?.running, data?.proxy?.port]);
    if (!force && signature === next) return;
    signature = next;
    const focusId = host.contains(document.activeElement) ? document.activeElement?.dataset.requestId : null;
    const scroll = host.querySelector('.gateway-route-history')?.scrollTop || 0;
    clear(host);
    const refresh = btn('刷新', 'sm', () => refreshNow(), '读取网关记录的请求路由');
    const followBox = el('input', {type:'checkbox', checked:follow});
    followBox.onchange = () => { follow = followBox.checked; paint(); };
    host.append(el('header', {class:'panelhead'}, el('h2', {}, '实时路由'),
      el('span', {class:'sub'}, 'Agent → 本地网关 → 上游模型'), el('span', {class:'sp'}), refresh),
      el('div', {class:'gateway-route-toolbar'},
        el('span', {class:'gateway-route-status ' + (current ? stateClass(current.status) : '')},
          current ? stateLabel(current.status) + ' · ' + time(current.started_at) : snapshot ? '等待请求' : '正在读取路由…'),
        el('label', {}, followBox, '跟随最新请求')));
    if (error) host.append(el('p', {class:'gateway-route-error', role:'status'}, '路由读取失败：' + error));
    const graph = el('div', {class:'gateway-route-graph', aria:{label:'请求流向'}});
    graph.append(node(current?.agent || '你的 Agent', current ? current.model || '未指定模型' : '接入后发起请求', '', '◇'),
      el('div', {class:'gateway-route-link ' + (active.has(current?.status) ? 'active' : ''), aria:{hidden:'true'}}, '→'),
      node('AgentCockpit', '127.0.0.1:' + (data?.proxy?.port || 8788), 'gateway-route-hub', '◈'),
      el('div', {class:'gateway-route-link ' + (active.has(current?.status) ? 'active' : ''), aria:{hidden:'true'}}, '→'));
    const attempts = Array.isArray(current?.attempts) ? current.attempts : [];
    const targets = el('ol', {class:'gateway-route-targets'});
    for (const [i, attempt] of attempts.entries()) {
      const status = stateLabel(attempt.state);
      const info = [attempt.provider, attempt.account].filter(Boolean).join(' · ');
      const item = el('li', {class:'gateway-route-attempt ' + stateClass(attempt.state)},
        el('div', {class:'gateway-route-attempt-heading'}, el('span', {class:'gateway-route-index'}, String(i + 1)),
          el('b', {}, attempt.model || current.model || '未指定模型'),
          el('span', {class:'gateway-route-attempt-state'}, status)),
        info ? el('div', {class:'gateway-route-subtitle'}, info) : null,
        attempt.reason ? el('div', {class:'gateway-route-reason'}, String(attempt.reason)) : null,
        attempt.status ? el('div', {class:'gateway-route-subtitle'}, 'HTTP ' + attempt.status) : null);
      targets.append(item);
    }
    if (!attempts.length) targets.append(el('li', {class:'gateway-route-empty'},
      current ? '尚未选择上游' : data?.proxy?.running ? '还没有请求' : '网关尚未启动'));
    graph.append(targets); host.append(graph);
    if (current?.error) host.append(el('p', {class:'gateway-route-error'}, String(current.error)));
    const stats = el('div', {class:'gateway-route-stats'});
    for (const [key, name] of [['total','请求'],['rerouted','改道'],['errors','Agent 收到的错误']]) {
      stats.append(el('div', {}, el('b', {}, count(snapshot?.[key])), el('span', {}, name)));
    }
    host.append(el('footer', {class:'gateway-route-footer'},
      el('p', {}, '统计从本次网关进程启动起累计，保留最近 64 条请求。账号已打码。'), stats));
    if (records.length) {
      const history = el('details', {class:'gateway-route-history-wrap', open:expanded});
      history.append(el('summary', {}, '最近请求 · ' + records.length));
      history.ontoggle = () => { if (history.isConnected) expanded = history.open; };
      const list = el('div', {class:'gateway-route-history'});
      records.forEach(r => {
        const row = el('button', {type:'button', class:'gateway-route-request' + (String(r.id) === selected ? ' selected' : ''),
          aria:{pressed:String(String(r.id) === selected)}},
          el('time', {}, time(r.started_at)), el('b', {}, r.agent || '未知 Agent'),
          el('span', {class:'gateway-route-request-model'}, r.model || '未指定模型'),
          el('span', {class:'gateway-route-request-state ' + stateClass(r.status)}, stateLabel(r.status)));
        row.dataset.requestId = String(r.id);
        row.onclick = () => { follow = false; selected = String(r.id); paint(); };
        list.append(row);
      });
      history.append(list); host.append(history); list.scrollTop = scroll;
      if (focusId) [...list.querySelectorAll('button')].find(b => b.dataset.requestId === focusId)?.focus({preventScroll:true});
    } else host.append(el('p', {class:'gateway-route-help'}, '从已接入的 Agent 发起请求后，这里会显示上游选择、尝试顺序与响应结果。'));
  }
  function schedule() {
    clearTimeout(timer); timer = null;
    if (visible && !document.hidden) timer = setTimeout(refreshNow, 2000);
  }
  async function refreshNow() {
    if (!visible || document.hidden || busy) return;
    clearTimeout(timer); timer = null;
    busy = true;
    const turn = generation;
    try {
      const result = await invoke('client_gateway', {app:'codex', name:'', keyId:'', model:'', op:'routes'});
      if (turn !== generation || !visible) return;
      if (!result || !Array.isArray(result.requests)) throw Error('当前服务未返回路由记录');
      snapshot = result; error = ''; paint();
    } catch (e) {
      if (turn === generation && visible) { error = String(e); paint(); }
    } finally { busy = false; schedule(); }
  }
  function setVisible(value, state) {
    data = state;
    if (visible === !!value) { if (visible) paint(); return; }
    visible = !!value; generation++;
    clearTimeout(timer); timer = null;
    if (visible) { paint(); refreshNow(); }
  }
  document.addEventListener('visibilitychange', () => {
    generation++; clearTimeout(timer); timer = null;
    if (visible && !document.hidden) refreshNow();
  });
  return {setVisible, refresh:refreshNow};
})();
