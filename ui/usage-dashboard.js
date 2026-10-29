/* Local usage statistics. No credentials or conversation text enter this view. */
(function (root) {
  'use strict';
  const COLORS = ['#679bf5', '#35b68e', '#a37be8', '#e7a354', '#e36c8b', '#42b7c7', '#a3b85b', '#b3937c'];
  const number = value => typeof value === 'number' && Number.isFinite(value) && value >= 0 ? value : 0;
  const known = value => typeof value === 'number' && Number.isFinite(value) && value >= 0;
  const dateKey = date => `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, '0')}-${String(date.getDate()).padStart(2, '0')}`;
  const validDate = value => typeof value === 'string' && /^\d{4}-\d{2}-\d{2}$/.test(value) && dateKey(new Date(`${value}T12:00:00`)) === value;
  function dateRange(range, now = new Date(), from = '', to = '') {
    const end = dateKey(now);
    if (range === 'custom') return { from: validDate(from) ? from : end, to: validDate(to) ? to : end };
    if (range === 'all') return { from: '', to: end };
    const start = new Date(now.getFullYear(), now.getMonth(), now.getDate());
    start.setDate(start.getDate() - Math.max(0, Number(range || 30) - 1));
    return { from: dateKey(start), to: end };
  }
  function normalizeSources(payload) {
    const result = new Map();
    for (const raw of Array.isArray(payload?.sources) ? payload.sources : []) {
      if (!raw || typeof raw !== 'object') continue;
      const id = String(raw.id || `${raw.app_id || 'unknown'}:${raw.instance_id || raw.instance_name || ''}:${raw.source || ''}`);
      const unit = String(raw.unit || 'tokens');
      const source = { ...raw, id, unit, app_id: String(raw.app_id || 'unknown'), app_name: String(raw.app_name || raw.app_id || '未知 Agent'), days: [] };
      const daily = new Map();
      for (const item of Array.isArray(raw.days) ? raw.days : []) {
        if (!item || !validDate(item.date)) continue;
        // Explicit exact/estimated fields take precedence over legacy tokens/credits totals.
        const explicit = known(item.exact_tokens) || known(item.estimated_tokens);
        const exact = explicit ? number(item.exact_tokens) : unit === 'tokens' || unit === 'token' ? number(item.tokens ?? item.credits) : 0;
        const estimated = number(item.estimated_tokens);
        const next = { ...item, date: item.date, exact_tokens: exact, estimated_tokens: estimated,
          native: unit === 'tokens' || unit === 'token' ? exact + estimated : number(item.credits ?? item.value),
          requests: known(item.requests) ? item.requests : null, sessions: known(item.sessions) ? item.sessions : null,
          models: Array.isArray(item.models) ? item.models : [] };
        // A source is an already aggregated daily series: repeated snapshots must not add twice.
        daily.set(item.date, next);
      }
      source.days = [...daily.values()].sort((a, b) => a.date.localeCompare(b.date));
      if (!result.has(id) || number(raw.fetched_at) >= number(result.get(id).fetched_at)) result.set(id, source);
    }
    return [...result.values()];
  }
  // The all-Agent overview uses globally deduplicated days. A specific Agent
  // uses its own deduplicated history; a selected source can show its own
  // history without adding it to the other sources. Shared history is not an
  // account bill, so keep its attribution visible in the coverage table.
  function scopedSources(payload, { app = '', sourceId = '' } = {}) {
    const raw = Array.isArray(payload?.sources) ? payload.sources : [];
    return normalizeSources({ sources: raw.map(s => {
      if (!s || typeof s !== 'object') return s;
      const rows = sourceId && s.id === sourceId ? s.source_days : !sourceId && app && s.app_id === app ? s.app_days : null;
      if (!Array.isArray(rows)) return s;
      const status = sourceId ? (s.source_status || (rows.length ? 'ready' : 'duplicate')) : (s.app_status || (rows.length ? 'ready' : 'duplicate'));
      return { ...s, days: rows, status, count_in_total: status === 'ready',
        reason: status === 'duplicate' ? '这份记录已由同一 Agent 的其他来源计入。' :
          (s.status === 'duplicate' ? '这份历史在总览中已去重，当前筛选单独展示；共享历史不代表账号独立账单。' : s.reason),
        coverage: `${s.coverage || ''} 当前筛选展示${sourceId ? '此来源自己的历史' : '此 Agent 内去重后的历史'}；共享历史不代表账号独立账单。` };
    }) });
  }
  const modelName = model => String(model?.model || model?.name || '未记录模型').trim() || '未记录模型';
  const modelKey = model => modelName(model).toLocaleLowerCase();
  function modelOptions(sources, app = '', sourceId = '') {
    const labels = new Map();
    for (const s of sources) {
      if ((app && s.app_id !== app) || (sourceId && s.id !== sourceId) || s.count_in_total === false || s.status === 'duplicate') continue;
      for (const day of s.days) for (const m of day.models || []) {
        if (!m || typeof m !== 'object') continue;
        if (!labels.has(modelKey(m))) labels.set(modelKey(m), modelName(m));
      }
    }
    return [...labels.entries()].sort((a, b) => a[1].localeCompare(b[1], 'zh-CN'));
  }
  function aggregate(sources, options = {}) {
    const { from = '', to = '', app = '', sourceId = '', model = '', includeEstimated = true, ratios = {}, dimension = 'agent' } = options;
    const selected = sources.filter(s => (!app || s.app_id === app) && (!sourceId || s.id === sourceId));
    const days = new Map(), groups = new Map(), sourceTotals = [];
    let exact = 0, estimated = 0, requests = 0, sessions = 0, hasRequests = false, hasSessions = false;
    const addGroup = (map, key, label, value) => {
      if (!(value > 0)) return;
      const g = map.get(key) || { key, label, tokens: 0 };
      g.tokens += value; map.set(key, g);
    };
    for (const source of selected) {
      const sums = { source, exact: 0, estimated: 0, native: 0, requests: 0, hasRequests: false };
      const ratio = number(Number(ratios[source.id]));
      for (const item of source.days) {
        if ((from && item.date < from) || (to && item.date > to)) continue;
        sums.native += item.native;
        // A vendor bill may cover calls already counted by a local token log.
        // Keep the bill visible while excluding every metric from combined totals.
        if (source.count_in_total === false || source.status === 'duplicate') continue;
        const converted = !model && source.unit !== 'tokens' && source.unit !== 'token' && !item.exact_tokens && !item.estimated_tokens ? number(item.native * ratio) : 0;
        let e = item.exact_tokens, est = item.estimated_tokens + converted, recordCount = item.requests, sessionCount = item.sessions;
        if (model) {
          // Only use an explicit model breakdown; credits and unlabelled history
          // cannot be assigned to a particular model.
          e = 0; est = 0; recordCount = null; sessionCount = null;
          let availableExact = item.exact_tokens, availableEstimated = item.estimated_tokens;
          for (const bucket of item.models) {
            if (!bucket || typeof bucket !== 'object') continue;
            const explicit = known(bucket.exact_tokens) || known(bucket.estimated_tokens);
            const be = Math.min(availableExact, explicit ? number(bucket.exact_tokens) : number(bucket.tokens));
            const bs = Math.min(availableEstimated, number(bucket.estimated_tokens));
            availableExact -= be; availableEstimated -= bs;
            if (modelKey(bucket) !== model) continue;
            e += be; est += bs;
            if (known(bucket.requests)) recordCount = number(recordCount) + bucket.requests;
          }
          if (!e && !est && recordCount === null) continue;
        }
        const visible = e + (includeEstimated ? est : 0);
        const day = days.get(item.date) || { date: item.date, exact: 0, estimated: 0, tokens: 0, requests: null, groups: new Map() };
        day.exact += e; day.estimated += est; day.tokens += visible;
        if (known(recordCount)) day.requests = number(day.requests) + recordCount;
        sums.exact += e; sums.estimated += est;
        exact += e; estimated += est;
        if (known(recordCount)) { requests += recordCount; sums.requests += recordCount; hasRequests = sums.hasRequests = true; }
        if (known(sessionCount)) { sessions += sessionCount; hasSessions = true; }
        if (dimension === 'model') {
          let attributed = 0;
          for (const model of item.models) {
            if (!model || typeof model !== 'object') continue;
            const modelExplicit = known(model.exact_tokens) || known(model.estimated_tokens);
            const modelExact = modelExplicit ? number(model.exact_tokens) : number(model.tokens);
            const modelTokens = modelExact + (includeEstimated ? number(model.estimated_tokens) : 0);
            // Keep malformed or overlapping model breakdowns from exceeding the source total.
            const count = Math.min(modelTokens, Math.max(0, visible - attributed));
            if (options.model && modelKey(model) !== options.model) continue;
            const label = modelName(model), key = modelKey(model);
            addGroup(groups, key, label, count); addGroup(day.groups, key, label, count); attributed += count;
          }
          addGroup(groups, 'unattributed', '未记录模型', visible - attributed);
          addGroup(day.groups, 'unattributed', '未记录模型', visible - attributed);
        } else {
          addGroup(groups, source.app_id, source.app_name, visible);
          addGroup(day.groups, source.app_id, source.app_name, visible);
        }
        days.set(item.date, day);
      }
      sourceTotals.push(sums);
    }
    const sortedDays = [...days.values()].sort((a, b) => a.date.localeCompare(b.date));
    return { sources: selected, sourceTotals, days: sortedDays, groups: [...groups.values()].sort((a, b) => b.tokens - a.tokens),
      exact, estimated, total: exact + (includeEstimated ? estimated : 0), requests: hasRequests ? requests : null,
      sessions: hasSessions ? sessions : null, activeDays: sortedDays.filter(d => d.tokens > 0).length,
      peak: Math.max(0, ...sortedDays.map(d => d.tokens)) };
  }
  function fillDays(days, from, to) {
    if (!validDate(from) || !validDate(to) || from > to) return [];
    const map = new Map(days.map(d => [d.date, d]));
    const out = [], cursor = new Date(`${from}T12:00:00`), end = new Date(`${to}T12:00:00`);
    while (cursor <= end && out.length < 3660) {
      const date = dateKey(cursor);
      out.push(map.get(date) || { date, exact: 0, estimated: 0, tokens: 0, requests: null, groups: new Map() });
      cursor.setDate(cursor.getDate() + 1);
    }
    return out;
  }
  const format = value => !known(value) ? '未记录' : value >= 1e9 ? `${(value / 1e9).toFixed(2)}B` : value >= 1e6 ? `${(value / 1e6).toFixed(2)}M` : value >= 1e3 ? `${(value / 1e3).toFixed(1)}K` : value.toLocaleString('zh-CN', { maximumFractionDigits: 1 });
  const full = value => number(value).toLocaleString('zh-CN', { maximumFractionDigits: 2 });
  const csvCell = value => `"${String(value ?? '').replace(/"/g, '""')}"`;
  function csvRows(data) {
    return [['日期', '实测 Token', '估算 Token', '图表 Token', '模型用量记录数'], ...data.days.map(d => [d.date, d.exact, d.estimated, d.tokens, d.requests])]
      .map(row => row.map(csvCell).join(',')).join('\r\n');
  }
  function monitorValues(monitor = {}, includeEstimated = true) {
    const exact = known(monitor.exact_tokens_per_minute) ? monitor.exact_tokens_per_minute : null;
    const estimated = known(monitor.estimated_tokens_per_minute) ? monitor.estimated_tokens_per_minute : null;
    return {
      rate: includeEstimated ? (known(monitor.tokens_per_minute) ? monitor.tokens_per_minute : null) : exact,
      exact, estimated,
      throughput: known(monitor.output_tokens_per_second) ? monitor.output_tokens_per_second : null,
      firstTokenSeconds: known(monitor.average_first_token_ms) ? monitor.average_first_token_ms / 1000 : null,
      interval: known(monitor.interval_seconds) ? monitor.interval_seconds : null,
      samples: number(monitor.samples), timingSamples: number(monitor.timing_samples),
      history: (Array.isArray(monitor.history) ? monitor.history : [])
        .filter(point => known(point.timestamp) && known(includeEstimated ? point.tokens_per_minute : point.exact_tokens_per_minute))
        .map(point => ({ timestamp: point.timestamp, rate: includeEstimated ? point.tokens_per_minute : point.exact_tokens_per_minute })),
    };
  }
  function scopedMonitor(monitor, sources, { app = '', sourceId = '', model = '' } = {}) {
    if (model) return { unavailable: '当前模型没有细分速率，暂不能按模型统计监测数据。' };
    if (!app && !sourceId) return monitor || {};
    const chosen = sources.filter(s => (!app || s.app_id === app) && (!sourceId || s.id === sourceId)
      && s.count_in_total !== false && s.status === 'ready');
    const lookup = sourceId ? (monitor?.by_selected_source || monitor?.by_source) : (monitor?.by_app_source || monitor?.by_source);
    const entries = chosen.map(s => lookup?.[s.id]).filter(Boolean);
    if (!entries.length) return { unavailable: '当前来源没有可用的监测采样。' };
    const sum = key => entries.every(e => known(e[key])) ? entries.reduce((n, e) => n + e[key], 0) : null;
    const duration = entries.reduce((n, e) => n + number(e.duration_ms), 0);
    const ttftSamples = entries.reduce((n, e) => n + number(e.ttft_samples), 0);
    const historyMaps = entries.map(e => new Map((e.history || []).map(point => [point.timestamp, point])));
    const history = [...historyMaps[0].keys()].filter(ts => historyMaps.every(map => map.has(ts))).sort((a,b) => a-b).map(timestamp => {
      const rows = historyMaps.map(map => map.get(timestamp));
      const add = key => rows.every(r => known(r[key])) ? rows.reduce((n, r) => n + r[key], 0) : null;
      return { timestamp, tokens_per_minute: add('tokens_per_minute'), exact_tokens_per_minute: add('exact_tokens_per_minute'), estimated_tokens_per_minute: add('estimated_tokens_per_minute') };
    });
    return { samples: monitor.samples, interval_seconds: monitor.interval_seconds, history,
      tokens_per_minute: sum('tokens_per_minute'), exact_tokens_per_minute: sum('exact_tokens_per_minute'),
      estimated_tokens_per_minute: sum('estimated_tokens_per_minute'),
      output_tokens_per_second: duration ? entries.reduce((n, e) => n + number(e.timed_output_tokens), 0) * 1000 / duration : null,
      timing_samples: entries.reduce((n, e) => n + number(e.timing_samples), 0),
      average_first_token_ms: ttftSamples ? entries.reduce((n, e) => n + number(e.ttft_ms), 0) / ttftSamples : null,
      note: monitor.note, unavailable: entries.length < chosen.length ? `当前 ${chosen.length} 个来源中仅 ${entries.length} 个有监测数据。` : '' };
  }
  // History is a set of recorded 5-minute buckets. Missing buckets are unknown,
  // not zero: do not fabricate points or draw a line through the gaps.
  function historicalRates(sources, { app = '', sourceId = '', model = '', range = '24h', includeEstimated = true, now = Date.now() / 1000 } = {}) {
    if (model) return { points: [], unavailable: '历史记录没有可靠的模型级速率，暂不能按模型筛选。' };
    const choices = { '24h': [86400, 300], '7d': [7 * 86400, 3600], '30d': [30 * 86400, 86400] };
    const [span, step] = choices[range] || choices['24h'];
    const field = sourceId ? 'source_historical_5m' : app ? 'app_historical_5m' : 'historical_5m';
    const start = now - span, groups = new Map();
    const coverageField = sourceId ? 'source_historical_coverage' : app ? 'app_historical_coverage' : 'historical_coverage';
    let untimed = 0, hasCoverage = false;
    for (const source of sources) {
      if ((app && source.app_id !== app) || (sourceId && source.id !== sourceId) || source.count_in_total === false || source.status !== 'ready') continue;
      const coverage = source[coverageField];
      if (coverage && typeof coverage === 'object') { hasCoverage = true; untimed += number(coverage.without_time_events); }
      for (const row of Array.isArray(source[field]) ? source[field] : []) {
        if (!known(row?.timestamp) || row.timestamp < start - 300 || row.timestamp > now) continue;
        // Unix-aligned buckets avoid inventing missing subintervals. A partial
        // first/last bucket is still divided by the full window and labelled.
        const timestamp = Math.floor(row.timestamp / step) * step;
        const item = groups.get(timestamp) || { timestamp, exact: 0, estimated: 0, events: 0 };
        item.exact += number(row.exact_tokens); item.estimated += number(row.estimated_tokens); item.events += number(row.events);
        groups.set(timestamp, item);
      }
    }
    const points = [...groups.values()].sort((a,b) => a.timestamp - b.timestamp).map(item => ({ ...item,
      rate: (item.exact + (includeEstimated ? item.estimated : 0)) / (step / 60), end: item.timestamp + step }));
    return { points, start, end: now, step, untimed: hasCoverage ? untimed : null,
      unavailable: points.length ? '' : '这个范围没有带具体时间的用量记录，无法推算历史速率。' };
  }
  function mergeSnapshot(previous, next) {
    if (next?.updating && !next.sources?.length && previous?.sources?.length) return { ...previous, updating: true };
    return next;
  }
  function pollDelay(active, pending, updating, interval) {
    return active && !pending ? (updating ? 2000 : number(interval) * 1000) : 0;
  }
  const api = { normalizeSources, scopedSources, aggregate, modelOptions, dateRange, fillDays, format, csvRows, monitorValues, scopedMonitor, historicalRates, mergeSnapshot, pollDelay };
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
  root.UsageDashboard = api;
  if (typeof document === 'undefined') return;

  const dom = (tag, cls, text) => { const n = document.createElement(tag); if (cls) n.className = cls; if (text !== undefined) n.textContent = text; return n; };
  const svg = (tag, attrs = {}, text) => { const n = document.createElementNS('http://www.w3.org/2000/svg', tag); for (const [key, value] of Object.entries(attrs)) n.setAttribute(key, String(value)); if (text !== undefined) n.textContent = text; return n; };
  const action = (text, fn, cls = 'btn sm') => { const b = dom('button', cls, text); b.type = 'button'; b.addEventListener('click', fn); return b; };
  let preferences = {};
  try { preferences = JSON.parse(localStorage.getItem('cockpit-usage-preferences') || '{}') || {}; } catch (_) { /* Broken preferences do not hide usage. */ }
  const state = { active: false, mounted: false, pending: false, timer: null, bridge: null, data: null, error: '',
    range: '30', from: dateKey(new Date()), to: dateKey(new Date()), app: '', sourceId: '', model: '', selectedDate: '', dimension: 'agent',
    includeEstimated: preferences.includeEstimated !== false, ratios: preferences.ratios || {}, interval: [0, 15, 30, 60].includes(preferences.interval) ? preferences.interval : 30,
    monitorRange: '24h', selectedRatePoint: null, scroller: null, parkedScroll: null };
  function persist() { try { localStorage.setItem('cockpit-usage-preferences', JSON.stringify({ ratios: state.ratios, includeEstimated: state.includeEstimated, interval: state.interval })); } catch (_) { /* Read-only webviews can still show charts. */ } }
  const byId = id => document.getElementById(id);
  function select(id, options, value, onChange) {
    const input = dom('select'); input.id = id;
    for (const [key, label] of options) { const opt = dom('option', '', label); opt.value = key; input.append(opt); }
    input.value = value; input.addEventListener('change', () => onChange(input.value)); return input;
  }
  function labelled(text, control) { const label = dom('label', 'ud-field'); label.append(dom('span', '', text), control); return label; }
  function mount() {
    const target = byId('usage-dashboard');
    if (!target || state.mounted) return;
    state.mounted = true;
    const tools = dom('div', 'ud-controls');
    tools.append(labelled('时间范围', select('ud-range', [['1', '今天'], ['7', '近 7 天'], ['30', '近 30 天'], ['90', '近 90 天'], ['365', '近一年'], ['all', '全部'], ['custom', '自定义']], state.range, v => { state.range = v; byId('ud-custom').hidden = v !== 'custom'; paint(true); })));
    const custom = dom('div', 'ud-custom'); custom.id = 'ud-custom'; custom.hidden = true;
    for (const [key, label] of [['from', '开始日期'], ['to', '结束日期']]) {
      const input = dom('input'); input.type = 'date'; input.value = state[key]; input.setAttribute('aria-label', label);
      input.addEventListener('change', () => { state[key] = input.value; paint(true); }); custom.append(input);
    }
    tools.append(custom,
      labelled('Agent', select('ud-agent', [['', '全部 Agent']], '', value => { state.app = value; state.sourceId = ''; updateSourceOptions(); paint(true); })),
      labelled('数据来源', select('ud-source', [['', '全部来源']], '', value => { state.sourceId = value; updateSourceOptions(); paint(true); })),
      labelled('模型', select('ud-model', [['', '全部模型']], '', value => { state.model = value; paint(true); })),
      labelled('拆分维度', select('ud-dimension', [['agent', 'Agent'], ['model', '模型']], state.dimension, value => { state.dimension = value; paint(true); })));
    const check = dom('input'); check.type = 'checkbox'; check.checked = state.includeEstimated;
    check.addEventListener('change', () => { state.includeEstimated = check.checked; persist(); paint(true); });
    const checkLabel = dom('label', 'ud-check'); checkLabel.append(check, '计入估算'); tools.append(checkLabel);
    const live = dom('div', 'ud-live-controls');
    live.append(labelled('自动刷新', select('ud-interval', [['15', '15 秒'], ['30', '30 秒'], ['60', '60 秒'], ['0', '暂停']], String(state.interval), value => { state.interval = Number(value); persist(); schedule(); updateStatus(); })), action('刷新数据', () => refresh(true)));
    const exportButton = action('导出 CSV', () => {
      const data = current(); const blob = new Blob(['\uFEFF' + csvRows(data)], { type: 'text/csv;charset=utf-8;' });
      const url = URL.createObjectURL(blob), link = dom('a'); link.href = url; link.download = `AgentCockpit-usage-${dateKey(new Date())}.csv`; link.click(); setTimeout(() => URL.revokeObjectURL(url), 1000);
    }); live.append(exportButton); tools.append(live);
    const status = dom('div', 'ud-status'); status.id = 'ud-status'; status.setAttribute('role', 'status'); status.setAttribute('aria-live', 'polite');
    const error = dom('div', 'ud-error'); error.id = 'ud-error'; error.hidden = true; error.setAttribute('role', 'alert');
    const body = dom('div', 'ud-body'); body.id = 'ud-body';
    body.addEventListener('focusout', () => setTimeout(paint, 0));
    target.append(tools, status, error, body);
    const scroller = target.closest('.pagebody');
    state.scroller = scroller; state.parkedScroll = scroller ? scroller.scrollTop : null;
    if (scroller) scroller.addEventListener('scroll', () => { state.parkedScroll = scroller.scrollTop; }, { passive: true });
  }
  function updateSourceOptions() {
    const sources = scopedSources(state.data, { app: state.app, sourceId: state.sourceId }), appControl = byId('ud-agent'), sourceControl = byId('ud-source'), modelControl = byId('ud-model');
    if (!appControl || !sourceControl || !modelControl) return;
    const apps = [...new Map(sources.map(s => [s.app_id, s.app_name])).entries()];
    const set = (control, options, value) => { control.replaceChildren(...options.map(([key, text]) => { const option = dom('option', '', text); option.value = key; return option; })); control.value = value; };
    set(appControl, [['', '全部 Agent'], ...apps], state.app);
    set(sourceControl, [['', '全部来源'], ...sources.filter(s => !state.app || s.app_id === state.app).map(s => [s.id, `${s.app_name} · ${s.instance_name || s.source || '本机'}`])], state.sourceId);
    const models = modelOptions(sources, state.app, state.sourceId);
    // Keep a selected model available even when the new Agent has no records for it.
    // The empty result then accurately shows that combination was not recorded.
    if (state.model && !models.some(([key]) => key === state.model)) models.push([state.model, `${state.model}（当前来源未记录）`]);
    set(modelControl, [['', '全部模型'], ...models], state.model);
  }
  function current(extra = {}) {
    return aggregate(scopedSources(state.data, { app: state.app, sourceId: state.sourceId }), { ...dateRange(state.range, new Date(), state.from, state.to), app: state.app, sourceId: state.sourceId, model: state.model, includeEstimated: state.includeEstimated, ratios: state.ratios, dimension: state.dimension, ...extra });
  }
  function updateStatus() {
    if (!state.mounted) return;
    const time = number(state.data?.generated_at), date = time ? new Date(time > 1e12 ? time : time * 1000) : null;
    const mode = state.pending || state.data?.updating ? '正在读取用量…' : state.interval ? `每 ${state.interval} 秒检查一次` : '已暂停自动刷新';
    byId('ud-status').textContent = `${mode}${date ? ` · 数据更新于 ${date.toLocaleString('zh-CN')}` : ''} · 未采集的数据不计作零消耗`;
    byId('ud-error').hidden = !state.error;
    byId('ud-error').textContent = state.error ? `刷新失败：${state.error}${state.data ? '。当前保留上次读取的数据。' : ''}` : '';
  }
  function schedule() {
    clearTimeout(state.timer); state.timer = null;
    const delay = pollDelay(state.active, state.pending, state.data?.updating, state.interval);
    if (delay) state.timer = setTimeout(() => refresh(false), delay);
  }
  async function refresh(force) {
    if (!state.active || state.pending || !state.bridge) return;
    state.pending = true; updateStatus(); clearTimeout(state.timer);
    try {
      const next = await state.bridge('usage_dashboard', { force: !!force });
      if (!next || !Array.isArray(next.sources)) throw new Error('统计服务没有返回用量来源');
      state.data = mergeSnapshot(state.data, next); state.error = '';
      if (state.active) { updateSourceOptions(); paint(); }
    } catch (error) { state.error = String(error?.message || error); }
    finally { state.pending = false; updateStatus(); schedule(); }
  }
  function card(title, subtitle) {
    const section = dom('section', 'ud-card'); const head = dom('div', 'ud-card-head'); head.append(dom('h2', '', title));
    if (subtitle) head.append(dom('p', '', subtitle)); section.append(head); return section;
  }
  function metric(label, value, detail, className = '') {
    const node = dom('div', `ud-metric ${className}`); node.append(dom('span', 'ud-metric-label', label), dom('strong', '', value), dom('span', 'ud-metric-detail', detail)); return node;
  }
  function empty(parent, text = '所选范围暂无可统计的 Token。可在下方查看数据覆盖情况。') { parent.append(dom('p', 'ud-empty', text)); }
  function colorsFor(groups) { return new Map(groups.map((g, i) => [g.key, COLORS[i % COLORS.length]])); }
  function legend(groups, colors) {
    const row = dom('div', 'ud-legend');
    for (const group of groups) { const item = dom('span'), dot = dom('i'); dot.style.background = colors.get(group.key); item.append(dot, group.label); row.append(item); }
    return row;
  }
  function chartDays(data) {
    const bounds = dateRange(state.range, new Date(), state.from, state.to);
    return fillDays(data.days, bounds.from || data.days[0]?.date || bounds.to, bounds.to);
  }
  function chartFrame(label, max, days, height = 242) {
    const width = 900, left = 66, right = 20, top = 15, bottom = height - 36;
    const graphic = svg('svg', { viewBox: `0 0 ${width} ${height}`, role: 'img', 'aria-label': label, class: 'ud-chart' });
    graphic.append(svg('title', {}, label));
    for (let i = 0; i <= 4; i++) {
      const y = bottom - i * (bottom - top) / 4;
      graphic.append(svg('line', { x1: left, x2: width - right, y1: y, y2: y, class: 'ud-gridline' }), svg('text', { x: left - 10, y: y + 4, 'text-anchor': 'end', class: 'ud-axis' }, format(max * i / 4)));
    }
    const stride = Math.max(1, Math.ceil(days.length / 9));
    days.forEach((d, i) => { if (i % stride === 0 || i === days.length - 1) graphic.append(svg('text', { x: left + (i + .5) * (width - left - right) / Math.max(days.length, 1), y: height - 10, 'text-anchor': 'middle', class: 'ud-axis' }, d.date.slice(5))); });
    return { graphic, width, left, right, top, bottom, plotWidth: width - left - right, plotHeight: bottom - top };
  }
  function selectedDayNote(data) {
    if (!state.selectedDate) return null;
    const day = data.days.find(d => d.date === state.selectedDate);
    const note = dom('div', 'ud-picked');
    note.append(dom('strong', '', state.selectedDate), dom('span', '', day
      ? `合计 ${full(day.tokens)} Token · 实测 ${full(day.exact)} · 估算 ${full(day.estimated)}`
      : '所选范围内没有这一天的记录。'), action('清除选点', () => { state.selectedDate = ''; paint(); }, 'btn sm'));
    if (day && day.groups.size) note.append(dom('small', '', [...day.groups.values()].map(g => `${g.label} ${full(g.tokens)}`).join(' · ')));
    return note;
  }
  function selectDay(date) { state.selectedDate = date; paint(); }
  function chartSelection(frame, days) {
    const { graphic, left, top, bottom, plotWidth } = frame;
    const step = plotWidth / days.length;
    const selectedIndex = days.findIndex(day => day.date === state.selectedDate);
    if (selectedIndex >= 0) {
      const x = left + (selectedIndex + .5) * step;
      graphic.append(svg('line', { x1: x, x2: x, y1: top, y2: bottom, class: 'ud-selected-line' }));
    }
    days.forEach((day, index) => {
      const hit = svg('rect', { x: left + index * step, y: top, width: step, height: bottom - top,
        fill: 'transparent', class: 'ud-chart-hit', tabindex: '0', role: 'button',
        'aria-label': `${day.date}，${full(day.tokens)} Token，点击查看详情` });
      hit.addEventListener('click', () => selectDay(day.date));
      hit.addEventListener('keydown', event => { if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); selectDay(day.date); } });
      graphic.append(hit);
    });
  }
  function barChart(data) {
    const section = card('每日 Token 消耗', state.includeEstimated ? '按来源堆叠，包含已标明的估算 Token。' : '只显示来源记录的实测 Token。');
    if (!data.total) { empty(section); return section; }
    const days = chartDays(data), max = Math.max(1, ...days.map(d => d.tokens)) * 1.08;
    const frame = chartFrame('每日 Token 堆叠柱状图', max, days), { graphic, left, bottom, plotWidth, plotHeight } = frame, colors = colorsFor(data.groups), step = plotWidth / days.length;
    days.forEach((day, i) => {
      let offset = 0;
      for (const group of data.groups) {
        const value = day.groups.get(group.key)?.tokens || 0; if (!value) continue;
        const height = value / max * plotHeight;
        const rect = svg('rect', { x: left + i * step + step * .13, y: bottom - offset - height, width: Math.max(.8, step * .74), height, fill: colors.get(group.key), rx: step > 8 ? 2 : 0 });
        rect.append(svg('title', {}, `${day.date} · ${group.label}\n${full(value)} Token\n当天实测 ${full(day.exact)}，估算 ${full(day.estimated)}`)); graphic.append(rect); offset += height;
      }
    });
    chartSelection(frame, days);
    section.append(graphic, legend(data.groups, colors)); const note = selectedDayNote(data); if (note) section.append(note); return section;
  }
  function lineChart(data) {
    const section = card('每日消耗曲线', '连续日期按天展示；空白记录日只表示已采集来源没有新增记录。');
    if (!data.total) { empty(section); return section; }
    const days = chartDays(data), max = Math.max(1, ...days.map(d => d.tokens)) * 1.08;
    const frame = chartFrame('每日 Token 折线图', max, days), { graphic, left, bottom, plotWidth, plotHeight } = frame, colors = colorsFor(data.groups);
    for (const group of data.groups) {
      const points = days.map((day, i) => [left + (i + .5) * plotWidth / days.length, bottom - (day.groups.get(group.key)?.tokens || 0) / max * plotHeight]);
      const path = svg('polyline', { points: points.map(p => p.join(',')).join(' '), fill: 'none', stroke: colors.get(group.key), 'stroke-width': 2.2, 'stroke-linejoin': 'round' });
      path.append(svg('title', {}, `${group.label} · ${full(group.tokens)} Token`)); graphic.append(path);
      days.forEach((day, i) => {
        const value = day.groups.get(group.key)?.tokens || 0;
        if (!value) return;
        const marker = svg('circle', { cx: points[i][0], cy: points[i][1], r: days.length < 100 ? 3 : 1.5, fill: colors.get(group.key) });
        marker.append(svg('title', {}, `${day.date} · ${group.label} · ${full(value)} Token`)); graphic.append(marker);
      });
    }
    chartSelection(frame, days);
    section.append(graphic, legend(data.groups, colors)); const note = selectedDayNote(data); if (note) section.append(note); return section;
  }
  function donut(data) {
    const section = card(state.dimension === 'model' ? '模型用量占比' : 'Agent 用量占比', '与当前日期、来源和估算选项保持一致。');
    if (!data.total) { empty(section); return section; }
    const colors = colorsFor(data.groups), chart = svg('svg', { viewBox: '0 0 240 220', role: 'img', 'aria-label': 'Token 用量环形图', class: 'ud-donut' });
    chart.append(svg('circle', { cx: 120, cy: 108, r: 75, fill: 'none', stroke: 'var(--track)', 'stroke-width': 27 }));
    let offset = 0; const circumference = 2 * Math.PI * 75;
    for (const group of data.groups) {
      const length = group.tokens / data.total * circumference;
      const arc = svg('circle', { cx: 120, cy: 108, r: 75, fill: 'none', stroke: colors.get(group.key), 'stroke-width': 27, 'stroke-dasharray': `${length} ${circumference - length}`, 'stroke-dashoffset': -offset, transform: 'rotate(-90 120 108)' });
      arc.append(svg('title', {}, `${group.label} · ${full(group.tokens)} Token · ${(group.tokens / data.total * 100).toFixed(1)}%`)); chart.append(arc); offset += length;
    }
    chart.append(svg('text', { x: 120, y: 108, 'text-anchor': 'middle', class: 'ud-donut-number' }, format(data.total)), svg('text', { x: 120, y: 129, 'text-anchor': 'middle', class: 'ud-axis' }, 'Token'));
    const list = dom('div', 'ud-share-list');
    for (const group of data.groups) {
      const row = dom('div', 'ud-share-row'), dot = dom('i'); dot.style.background = colors.get(group.key);
      row.append(dot, dom('span', '', group.label), dom('strong', '', `${(group.tokens / data.total * 100).toFixed(1)}%`), dom('small', '', `${format(group.tokens)} Token`)); list.append(row);
    }
    section.append(chart, list); return section;
  }
  function heatmap() {
    const end = new Date(), start = new Date(end.getFullYear(), end.getMonth(), end.getDate()); start.setDate(start.getDate() - 364);
    const data = current({ from: dateKey(start), to: dateKey(end) }), map = new Map(data.days.map(d => [d.date, d]));
    const section = card('年度用量热力图', '最近 365 天，跟随 Agent、模型、来源和估算选项。点击日期查看数值，不改变上方时间范围。');
    const scroll = dom('div', 'ud-heat-scroll'), grid = dom('div', 'ud-heat');
    const offset = (start.getDay() + 6) % 7;
    for (let i = 0; i < offset; i++) grid.append(dom('span', 'ud-heat-pad'));
    const cursor = new Date(start), peak = Math.max(1, data.peak);
    while (cursor <= end) {
      const date = dateKey(cursor), day = map.get(date), value = day?.tokens || 0;
      const level = value ? Math.max(1, Math.min(4, Math.ceil(value / peak * 4))) : 0;
      const button = action('', () => selectDay(date), `ud-heat-cell ud-level-${level}${state.selectedDate === date ? ' ud-selected' : ''}`);
      button.title = `${date}\n实测 ${full(day?.exact)} Token\n估算 ${full(day?.estimated)} Token${day ? '' : '\n没有采集记录'}`;
      button.setAttribute('aria-label', button.title); grid.append(button); cursor.setDate(cursor.getDate() + 1);
    }
    scroll.append(grid); section.append(scroll);
    const foot = dom('div', 'ud-heat-foot'); foot.append(dom('span', '', `${dateKey(start)} 至 ${dateKey(end)} · ${data.activeDays} 个活跃日`));
    const scale = dom('span', 'ud-heat-scale', '少'); for (let i = 0; i < 5; i++) scale.append(dom('i', `ud-level-${i}`)); scale.append('多'); foot.append(scale); section.append(foot);
    const note = selectedDayNote(data); if (note) section.append(note); return section;
  }
  function monitoring() {
    const sources = scopedSources(state.data, { app: state.app, sourceId: state.sourceId });
    const m = scopedMonitor(state.data?.monitor, sources, { app: state.app, sourceId: state.sourceId, model: state.model }), values = monitorValues(m, state.includeEstimated);
    const scope = state.sourceId ? '所选来源' : state.app ? '所选 Agent' : '全部 Agent';
    const section = card('用量监测', `${scope}；消耗速率来自日志新增记录，回复吞吐和首字时间来自有计时的历史请求。`);
    if (m.unavailable) section.append(dom('p', 'ud-note', m.unavailable));
    const persistence = state.data?.monitor?.history_persistence;
    if (persistence?.ok === false) section.append(dom('p', 'ud-error', `实时采样历史保存失败，当前仍可观察速率，重启后可能丢失历史。${persistence.reason || ''}`));
    const strip = dom('div', 'ud-monitor-grid');
    strip.append(metric('最近采样 Token / 分钟', format(values.rate), known(values.rate)
      ? `实测 ${format(values.exact)} · 估算 ${format(values.estimated)}${state.includeEstimated ? '' : '（未计入）'}` : '等待两次有效采样'),
      metric('回复平均吞吐', known(values.throughput) ? `${format(values.throughput)} Token/秒` : '未记录', `${full(values.timingSamples)} 个有计时的请求，含首字等待`),
      metric('平均首字时间', known(values.firstTokenSeconds) ? `${format(values.firstTokenSeconds)} 秒` : '未记录', '仅统计保存了首字时间的请求'),
      metric('采样窗口', known(values.interval) ? `${full(values.interval)} 秒` : '等待采样', `${full(values.samples)} 次监测采样`));
    section.append(strip);
    const controls = dom('div', 'ud-rate-controls');
    controls.append(labelled('速率时间范围', select('ud-rate-range', [['live', '实时采样'], ['24h', '近 24 小时'], ['7d', '近 7 天'], ['30d', '近 30 天']], state.monitorRange, value => {
      state.monitorRange = value; parkScroll(); paint(true);
    })));
    section.append(controls);
    const historical = state.monitorRange !== 'live';
    const result = historical ? historicalRates(sources, { app: state.app, sourceId: state.sourceId, model: state.model,
      range: state.monitorRange, includeEstimated: state.includeEstimated }) : null;
    const points = historical ? result.points : values.history;
    if (points.length) {
      const first = historical ? result.start : points[0].timestamp;
      const last = historical ? result.end : points[points.length - 1].timestamp;
      const width = 900, height = 150, left = 66, bottom = 116, plotWidth = 814, plotHeight = 92;
      const graphic = svg('svg', { viewBox: `0 0 ${width} ${height}`, role: 'img', 'aria-label': historical ? '根据历史记录推算的分段平均消耗速率，Token 每分钟' : '实时采样消耗速率曲线，Token 每分钟', class: 'ud-chart ud-rate-chart' });
      // tip 口径必须与当前图表一致：选点跨档保留时（选中的 5 分钟点并入小时/天分桶），
      // 注释按所在分桶重算，页面高度不因切换范围而塌掉，滚动位置才守得住。
      const stepSeconds = historical ? result.step : 0;
      const tipOf = (point, span) => {
        const when = new Date(point.timestamp * 1000).toLocaleString('zh-CN');
        return span ? `${when} 起 ${span / 60} 分钟\n平均 ${full(point.rate)} Token/分钟\n实测 ${full(point.exact)} · 估算 ${full(point.estimated)} Token\n${full(point.events)} 条用量记录` : `${when} · ${full(point.rate)} Token/分钟`;
      };
      const max = Math.max(1, ...points.map(p => p.rate)) * 1.1;
      for (let i = 0; i <= 2; i++) {
        const y = bottom - i * plotHeight / 2;
        graphic.append(svg('line', { x1: left, x2: left + plotWidth, y1: y, y2: y, class: 'ud-gridline' }), svg('text', { x: left - 10, y: y + 4, 'text-anchor': 'end', class: 'ud-axis' }, format(max * i / 2)));
      }
      const coordinates = points.map(p => [left + Math.max(0, Math.min(1, (p.timestamp - first) / Math.max(1, last - first))) * plotWidth, bottom - p.rate / max * plotHeight]);
      if (!historical) graphic.append(svg('polyline', { points: coordinates.map(p => p.join(',')).join(' '), fill: 'none', stroke: COLORS[0], 'stroke-width': 2.2 }));
      else for (let i = 1; i < points.length; i++) {
        if (points[i].timestamp - points[i - 1].timestamp <= result.step) graphic.append(svg('line', {
          x1: coordinates[i - 1][0], y1: coordinates[i - 1][1], x2: coordinates[i][0], y2: coordinates[i][1], stroke: COLORS[0], 'stroke-width': 2.2 }));
      }
      for (let i = 0; i < points.length; i++) {
        const point = points[i];
        const tip = tipOf(point, stepSeconds);
        const dot = svg('circle', { cx: coordinates[i][0], cy: coordinates[i][1], r: 3.5, fill: COLORS[0], tabindex: 0, role: 'button', 'aria-label': tip, class: 'ud-rate-point' });
        dot.append(svg('title', {}, tip));
        const choose = () => { state.selectedRatePoint = { timestamp: point.timestamp }; paint(); };
        dot.addEventListener('click', choose); dot.addEventListener('keydown', event => { if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); choose(); } });
        graphic.append(dot);
      }
      for (const [ts, x, anchor] of [[first, left, 'start'], [last, left + plotWidth, 'end']]) graphic.append(svg('text', { x, y: height - 10, 'text-anchor': anchor, class: 'ud-axis' }, new Date(ts * 1000).toLocaleString('zh-CN')));
      section.append(graphic);
      const picked = state.selectedRatePoint && (points.find(p => p.timestamp === state.selectedRatePoint.timestamp)
        || (stepSeconds ? points.find(p => state.selectedRatePoint.timestamp > p.timestamp && state.selectedRatePoint.timestamp < p.timestamp + stepSeconds) : null));
      if (picked) section.append(dom('p', 'ud-rate-picked', tipOf(picked, stepSeconds)));
      section.append(dom('p', 'ud-note', historical
        ? `横轴：本地时间；纵轴：每分钟平均 Token。按日志时间将已记录消耗汇总到 ${result.step / 60} 分钟时间段并除以该时长；没有记录的时间段不画线。未记录的调用可能使数值偏低，头尾未满时段仍按完整时段计算。日志可能延迟写入，平均速率不代表模型实时输出速度。`
        : '横轴：本地采样时间；纵轴：两次采样之间每分钟观察到的 Token 消耗。日志集中写入可能产生短时峰值；采样可能因程序关闭而中断。'));
    } else section.append(dom('p', 'ud-note', historical ? result.unavailable : (m.unavailable || '还没有两次有效采样；等待后会显示实时曲线。')));
    if (historical && result && result.untimed > 0) section.append(dom('p', 'ud-note', `所选来源的全部可读取历史中，有 ${full(result.untimed)} 条记录缺少具体时刻，不能分配到速率时间段。`));
    if (m.note || m.reason) section.append(dom('p', 'ud-note', String(m.note || m.reason)));
    return section;
  }
  function coverage(data) {
    const section = card('来源覆盖与换算', '实测 Token 是客户端保存的 usage；估算 Token 来自可见文本或用户填写的换算系数。原始用量使用来源本身的单位。');
    section.append(dom('p', 'ud-note', '可换算的 credits 来源在“每单位折合 Token”列填写数字；有本地 Token 记录且账单可能重叠时，该行只显示原始 credits，不能重复换算。模型筛选只覆盖记录了模型名称的 Token。'));
    const wrap = dom('div', 'ud-table-scroll'), table = dom('table', 'ud-table'), head = dom('thead'), hr = dom('tr');
    for (const text of ['Agent / 来源', '实测 Token', '估算 Token', '原始用量', '每单位折合 Token', '记录范围与未覆盖原因']) hr.append(dom('th', '', text)); head.append(hr); table.append(head);
    const body = dom('tbody');
    for (const sums of data.sourceTotals) {
      const s = sums.source, row = dom('tr'), name = dom('td'); name.append(dom('strong', '', s.app_name), dom('small', '', s.instance_name || ''), dom('small', '', s.source || '本机记录'));
      const rateCell = dom('td');
      if (s.count_in_total === false || s.status === 'duplicate') {
        rateCell.append(dom('span', 'ud-muted', '未加入总量'), dom('small', '', s.days.length ? '可能与本地记录重叠，不提供重复换算' : '没有可统计的记录'));
      } else if (s.unit !== 'tokens' && s.unit !== 'token') {
        const input = dom('input', 'ud-ratio'); input.type = 'number'; input.min = '0'; input.step = 'any'; input.placeholder = '未设置'; input.value = state.ratios[s.id] || ''; input.setAttribute('aria-label', `${s.app_name} 每 ${s.unit} 折算 Token 数`);
        input.addEventListener('change', () => { const n = Number(input.value); if (input.value && (!Number.isFinite(n) || n <= 0)) { input.setCustomValidity('请填写大于零的换算系数，或留空。'); input.reportValidity(); return; } input.setCustomValidity(''); if (!input.value) delete state.ratios[s.id]; else state.ratios[s.id] = n; persist(); paint(); });
        rateCell.append(input, dom('small', '', '手动系数，结果计入估算'));
      } else rateCell.append(dom('span', 'ud-muted', '直接记录'));
      const status = dom('td');
      const coverageText = typeof s.coverage === 'string' ? s.coverage : s.coverage?.description || s.coverage?.note || '';
      const reason = s.reason || coverageText || (s.days.length ? '已读取本机记录' : '暂无可用记录');
      status.append(dom('span', `ud-source-status ${s.days.length ? 'ready' : ''}`, s.days.length ? `${s.days.length} 天记录` : '未覆盖'), dom('small', '', reason));
      if (coverageText && coverageText !== reason) status.append(dom('small', '', coverageText));
      if (s.scope === 'device') status.append(dom('small', '', '本机共用记录，未按登录账号拆分'));
      if (s.estimate_method || s.estimated_method || s.estimation_method) status.append(dom('small', '', String(s.estimate_method || s.estimated_method || s.estimation_method)));
      if (s.status === 'cached') status.append(dom('small', '', `使用上次额度刷新的历史${known(s.fetched_at) ? `，${new Date(s.fetched_at * 1000).toLocaleString('zh-CN')}` : ''}`));
      const hasRecords = !!s.days.length;
      row.append(name, dom('td', 'ud-number', format(hasRecords ? sums.exact : null)), dom('td', 'ud-number', format(hasRecords ? sums.estimated : null)), dom('td', '', hasRecords ? `${format(sums.native)} ${s.unit}${state.model ? '（来源全部模型）' : ''}` : '未记录'), rateCell, status); body.append(row);
    }
    table.append(body); wrap.append(table); section.append(wrap);
    if (!data.sourceTotals.length) empty(section, '暂未发现数据来源。刷新后会列出已登记 Agent 的读取状态。');
    const note = dom('details', 'ud-explanation'); note.id = 'ud-explanation'; note.append(dom('summary', '', 'credits、自定义接口与 Token 的统计口径'));
    note.append(dom('p', '', '实测 Token 来自模型响应、客户端数据库或会话日志保存的 usage；估算 Token 通过可见消息文本近似计算，可能漏掉隐藏提示和重复上下文。原始用量保留来源本身的单位：Token 日志的数字包括实测和估算，credits 账单则显示 credits。'),
      dom('p', '', 'credits 是厂商的计费单位，可能随模型、输入输出比例、缓存和套餐变化。只有独立的 credits 来源会出现“每单位折合 Token”输入框；填写有依据的系数后，其换算结果会单独列为估算 Token。为防止双算，与本地 Token 可能重叠的账单不提供换算输入。'),
      dom('p', '', '自定义接口返回的 usage 如果写入了本机日志，就能进入实测统计；只有可见对话文本时，估算值仅覆盖文本，不包含隐藏提示、工具上下文和缓存。来源既没有 usage 也没有可读文本时，页面会标明未覆盖。'),
      dom('p', '', '官方 credits 热力图反映官方账单覆盖的调用。第三方直连请求是否被记入，需要以该产品实际保存的记录为准，不能通过 credits 数字推断。'));
    section.append(note); return section;
  }
  function dailyTable(data) {
    const details = dom('details', 'ud-card ud-daily'); details.id = 'ud-daily'; details.append(dom('summary', '', `每日明细 · ${data.days.length} 天`));
    const wrap = dom('div', 'ud-table-scroll'), table = dom('table', 'ud-table'), head = dom('tr');
    for (const label of ['日期', '实测 Token', '估算 Token', '图表 Token']) head.append(dom('th', '', label)); table.append(head);
    for (const day of [...data.days].reverse()) { const row = dom('tr'); for (const text of [day.date, full(day.exact), full(day.estimated), full(day.tokens)]) row.append(dom('td', '', text)); table.append(row); }
    wrap.append(table); details.append(wrap); return details;
  }
  // focus() 会把视口滚向控件本身，且可能推迟到渲染步才生效，落在 change 处理器捕获
  // scrollTop 之后；先把视口停回用户原本的位置，下一帧再核一次，paint(true) 的还原
  // 才不会把这次错位固化下来（同 app.js restoreScroll 的两道护栏）。
  function parkScroll() {
    const scroller = state.scroller;
    if (!scroller || state.parkedScroll == null) return;
    const top = state.parkedScroll;
    const reset = () => { if (Math.abs(scroller.scrollTop - top) > 0.5) scroller.scrollTop = top; };
    reset();
    requestAnimationFrame(reset);
  }
  function paint(force = false) {
    if (!state.mounted || !state.active) return;
    const rootNode = byId('ud-body'), scroller = rootNode.closest('.pagebody'), scrollTop = scroller?.scrollTop || 0;
    // 这道护栏只保护自动刷新：轮询触发的 paint() 不得打断正在输入的控件。
    // 用户主动的控件变更一律走 paint(true)，否则上次重聚焦的控件会让重绘被误吞。
    const focused = rootNode.contains(document.activeElement) && document.activeElement?.matches('input, select');
    if (focused && !force) { updateStatus(); return; }
    const focusedId = focused && document.activeElement.id;
    const expanded = [...rootNode.querySelectorAll('details[open]')].map(node => node.id);
    const data = current(), metrics = dom('div', 'ud-metrics');
    metrics.append(metric('累计 Token', format(data.total), state.includeEstimated ? `实测 ${format(data.exact)} · 估算 ${format(data.estimated)}` : `仅实测 · ${format(data.estimated)} 估算未计入`, 'ud-metric-primary'),
      metric('模型用量记录', format(data.requests), '按模型调用或用量事件计数，口径随来源变化'),
      metric('单日峰值', format(data.peak), `${data.activeDays} 个活跃日`),
      metric('数据覆盖', `${data.sources.filter(s => s.days.length).length} / ${data.sources.length}`, '有历史记录的来源 / 已检查来源'));
    const chartGrid = dom('div', 'ud-chart-grid'); chartGrid.append(barChart(data), donut(data));
    rootNode.replaceChildren(metrics, chartGrid, lineChart(data), heatmap(), monitoring(), coverage(data), dailyTable(data));
    for (const id of expanded) if (byId(id)) byId(id).open = true;
    if (state.range === 'custom' && state.from > state.to) rootNode.prepend(dom('p', 'ud-error', '开始日期需要早于或等于结束日期。'));
    if (!state.data) rootNode.prepend(dom('p', 'ud-note', '正在读取各 Agent 的本机用量记录，首次读取可能需要一些时间。'));
    if (focusedId) byId(focusedId)?.focus({ preventScroll: true });
    if (scroller) scroller.scrollTop = scrollTop;
    updateStatus();
  }
  api.setVisible = function (active, bridge) {
    state.bridge = bridge || state.bridge;
    const changed = state.active !== !!active; state.active = !!active;
    if (!active) { clearTimeout(state.timer); state.timer = null; return; }
    mount();
    if (changed) { paint(); refresh(false); }
  };
  api.refresh = refresh;
  api.selectApp = function (app) {
    state.app = String(app || ''); state.sourceId = '';
    if (state.mounted) { updateSourceOptions(); paint(); }
  };
})(typeof globalThis === 'undefined' ? this : globalThis);
