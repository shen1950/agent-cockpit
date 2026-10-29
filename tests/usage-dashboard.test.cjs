'use strict';
const assert = require('node:assert/strict');
const test = require('node:test');
const usage = require('../ui/usage-dashboard.js');

function source(id, unit, days, extra = {}) {
  return { id, app_id: id, app_name: id, unit, days, ...extra };
}
function normalized(sources) { return usage.normalizeSources({ sources }); }

test('real tokens, visible-text estimates and optional credits conversion stay separate', () => {
  const sources = normalized([
    source('a', 'tokens', [{ date: '2026-10-28', exact_tokens: 100, estimated_tokens: 25, tokens: 999, requests: 2 }]),
    source('b', 'credits', [{ date: '2026-10-28', credits: 4 }]),
  ]);
  assert.equal(usage.aggregate(sources).total, 125);
  const result = usage.aggregate(sources, { ratios: { b: 20 } });
  assert.equal(result.exact, 100);
  assert.equal(result.estimated, 105);
  assert.equal(result.total, 205);
  assert.equal(result.requests, 2);
  assert.equal(usage.aggregate(sources, { ratios: { b: 20 }, includeEstimated: false }).total, 100);
});

test('overlapping vendor bills remain visible without double counting converted tokens or records', () => {
  const sources = normalized([
    source('native', 'tokens', [{ date: '2026-10-28', exact_tokens: 120, requests: 3 }]),
    source('bill', 'credits', [{ date: '2026-10-28', credits: 8, requests: 3 }], { count_in_total: false }),
    source('alias', 'tokens', [{ date: '2026-10-28', tokens: 120, requests: 3 }], { status: 'duplicate' }),
  ]);
  const result = usage.aggregate(sources, { ratios: { bill: 1000 } });
  assert.equal(result.total, 120);
  assert.equal(result.requests, 3);
  assert.equal(result.sourceTotals[1].native, 8);
  assert.equal(result.sourceTotals[1].estimated, 0);
  assert.deepEqual(result.groups.map(g => g.key), ['native']);
});

test('newest source snapshot wins and repeated day snapshots do not add', () => {
  const sources = normalized([
    source('same', 'token', [{ date: '2026-10-28', credits: 200 }], { fetched_at: 10 }),
    source('same', 'token', [{ date: '2026-10-28', credits: 30 }, { date: '2026-10-28', credits: 40 }], { fetched_at: 20 }),
    source('same', 'token', [{ date: '2026-10-28', credits: 500 }], { fetched_at: 5 }),
  ]);
  assert.equal(sources.length, 1);
  assert.equal(usage.aggregate(sources).total, 40);
});

test('dates, Agent filters and unknown request counts preserve their meaning', () => {
  const sources = normalized([
    source('a', 'tokens', [{ date: '2026-02-30', tokens: 999 }, null, { date: '2026-10-28', tokens: 10 }, { date: '2026-10-29', tokens: 20 }]),
    source('b', 'tokens', [{ date: '2026-10-29', tokens: 30, requests: 0 }]), null,
  ]);
  const result = usage.aggregate(sources, { from: '2026-10-29', to: '2026-10-29', app: 'a' });
  assert.equal(result.total, 20);
  assert.equal(result.requests, null);
  assert.equal(result.days[0].requests, null);
  assert.equal(usage.aggregate(sources, { app: 'b' }).requests, 0);
  assert.match(usage.csvRows(result), /"20","0","20",""$/);
  assert.equal(usage.aggregate(sources, { from: '2026-10-30', to: '2026-10-29' }).total, 0);
});

test('model breakdown cannot exceed source tokens and leaves unattributed remainder', () => {
  const sources = normalized([source('a', 'tokens', [{ date: '2026-10-29', exact_tokens: 100, estimated_tokens: 20,
    models: [{ model: 'one', exact_tokens: 60 }, { model: 'two', exact_tokens: 20 }] }])]);
  const model = usage.aggregate(sources, { dimension: 'model' });
  assert.equal(model.groups.reduce((sum, row) => sum + row.tokens, 0), 120);
  assert.equal(model.groups.find(row => row.key === 'unattributed').tokens, 40);
  sources[0].days[0].models.push({ model: 'overlap', tokens: 1000 });
  assert.equal(usage.aggregate(sources, { dimension: 'model' }).groups.reduce((sum, row) => sum + row.tokens, 0), 120);
});

test('model filter combines the same recorded model across Agents and never assigns unlabelled tokens', () => {
  const sources = normalized([
    source('zcode', 'tokens', [{ date: '2026-10-29', exact_tokens: 100, estimated_tokens: 20,
      models: [{ model: 'GLM-5.3-Flash', exact_tokens: 40, estimated_tokens: 5, requests: 2 },
        { model: 'other', exact_tokens: 20, estimated_tokens: 5 }] }], { status: 'ready' }),
    source('workbuddy', 'tokens', [{ date: '2026-10-29', exact_tokens: 60,
      models: [{ model: 'glm-5.3-flash', exact_tokens: 60, requests: 1 }] }], { status: 'ready' }),
    source('credits', 'credits', [{ date: '2026-10-29', credits: 50 }]),
  ]);
  const options = usage.modelOptions(sources);
  assert.equal(options.filter(([key]) => key === 'glm-5.3-flash').length, 1);
  const all = usage.aggregate(sources, { model: 'glm-5.3-flash', dimension: 'agent', ratios: { credits: 100 } });
  assert.equal(all.exact, 100); assert.equal(all.estimated, 5); assert.equal(all.requests, 3);
  assert.equal(all.groups.length, 2);
  assert.equal(usage.aggregate(sources, { model: 'glm-5.3-flash', app: 'zcode' }).total, 45);
  assert.equal(usage.aggregate(sources, { model: 'other', app: 'workbuddy' }).total, 0);
  assert.equal(usage.aggregate(sources, { model: 'glm-5.3-flash', includeEstimated: false }).total, 100);
  const byModel = usage.aggregate(sources, { model: 'glm-5.3-flash', dimension: 'model' });
  assert.equal(byModel.groups.reduce((n, g) => n + g.tokens, 0), byModel.total);
});

test('Agent and source histories remain viewable while the all-Agent total stays deduplicated', () => {
  const day = tokens => [{ date: '2026-10-29', exact_tokens: tokens }];
  const payload = { sources: [
    source('codex', 'tokens', day(100), { status: 'ready', app_id: 'codex', app_days: day(100), source_days: day(100), app_status: 'ready', source_status: 'ready' }),
    source('desktop-a', 'tokens', [], { status: 'duplicate', count_in_total: false, app_id: 'codex-desktop', app_days: day(100), source_days: day(100), app_status: 'ready', source_status: 'ready' }),
    source('desktop-b', 'tokens', [], { status: 'duplicate', count_in_total: false, app_id: 'codex-desktop', app_days: [], source_days: day(100), app_status: 'duplicate', source_status: 'ready' }),
    source('bill', 'credits', [{ date: '2026-10-29', credits: 10 }], { count_in_total: false, app_id: 'codex-desktop' }),
  ] };
  assert.equal(usage.aggregate(usage.scopedSources(payload), { ratios: { bill: 100 } }).total, 100);
  const desktop = usage.scopedSources(payload, { app: 'codex-desktop' });
  assert.equal(usage.aggregate(desktop, { app: 'codex-desktop', ratios: { bill: 100 } }).total, 100);
  assert.equal(desktop.find(s => s.id === 'desktop-b').status, 'duplicate');
  const single = usage.scopedSources(payload, { app: 'codex-desktop', sourceId: 'desktop-b' });
  assert.equal(usage.aggregate(single, { app: 'codex-desktop', sourceId: 'desktop-b' }).total, 100);
  assert.match(single.find(s => s.id === 'desktop-b').coverage, /共享历史/);
});

test('monitor respects source filters, missing timestamps and unavailable model granularity', () => {
  const sources = normalized([source('a', 'tokens', [], { status: 'ready' }), source('b', 'tokens', [], { status: 'ready', app_id: 'a' })]);
  const sample = (rate, history) => ({ tokens_per_minute: rate, exact_tokens_per_minute: rate,
    estimated_tokens_per_minute: 0, timed_output_tokens: 100, duration_ms: 2000, timing_samples: 1,
    ttft_ms: 400, ttft_samples: 1, history });
  const point = (timestamp, rate) => ({ timestamp, tokens_per_minute: rate, exact_tokens_per_minute: rate, estimated_tokens_per_minute: 0 });
  const monitor = { samples: 3, by_source: { a: sample(10, [point(1, 2), point(2, 10)]), b: sample(20, [point(2, 20)]) } };
  const group = usage.scopedMonitor(monitor, sources, { app: 'a' });
  assert.equal(group.tokens_per_minute, 30); assert.equal(group.output_tokens_per_second, 50);
  assert.equal(group.average_first_token_ms, 400); assert.deepEqual(group.history.map(p => p.timestamp), [2]);
  assert.equal(usage.scopedMonitor(monitor, sources, { app: 'a', sourceId: 'a' }).tokens_per_minute, 10);
  assert.match(usage.scopedMonitor(monitor, sources, { model: 'glm' }).unavailable, /模型/);
});

test('historical rates use timestamped buckets, selected scope and no fabricated gaps', () => {
  const now = 1800000000, earlier = now - 1200;
  const payload = { sources: [
    source('codex', 'tokens', [], { status: 'ready', app_id: 'codex', historical_5m: [{ timestamp: earlier, exact_tokens: 1500, estimated_tokens: 300, events: 2 }] }),
    source('desktop', 'tokens', [], { status: 'duplicate', count_in_total: false, app_id: 'codex-desktop',
      app_status: 'ready', source_status: 'ready', app_days: [], source_days: [],
      historical_5m: [], app_historical_5m: [{ timestamp: earlier, exact_tokens: 1500, estimated_tokens: 300, events: 2 }],
      source_historical_5m: [{ timestamp: earlier, exact_tokens: 1500, estimated_tokens: 300, events: 2 }] }),
  ] };
  const all = usage.historicalRates(usage.scopedSources(payload), { range: '24h', now });
  assert.equal(all.points.length, 1);
  assert.equal(all.points[0].rate, 360);
  assert.equal(all.points[0].events, 2);
  const scoped = usage.scopedSources(payload, { app: 'codex-desktop' });
  assert.equal(usage.historicalRates(scoped, { app: 'codex-desktop', now, includeEstimated: false }).points[0].rate, 300);
  const single = usage.scopedSources(payload, { app: 'codex-desktop', sourceId: 'desktop' });
  assert.equal(usage.historicalRates(single, { app: 'codex-desktop', sourceId: 'desktop', now }).points[0].rate, 360);
  assert.equal(usage.historicalRates(scoped, { app: 'codex-desktop', range: '7d', now }).points[0].rate, 30);
  assert.equal(usage.historicalRates(scoped, { app: 'codex-desktop', range: '30d', now }).points[0].rate, 1.25);
  assert.match(usage.historicalRates(scoped, { model: 'foo', now }).unavailable, /模型/);
  assert.equal(usage.historicalRates([], { now }).points.length, 0);
});

test('per-Agent and per-source live rates select their own accounting views', () => {
  const sources = normalized([source('desktop', 'tokens', [], { status: 'ready', app_id: 'chatgpt' })]);
  const row = value => ({ tokens_per_minute: value, exact_tokens_per_minute: value, estimated_tokens_per_minute: 0, history: [] });
  const monitor = { by_source: { desktop: row(1) }, by_app_source: { desktop: row(2) }, by_selected_source: { desktop: row(3) } };
  assert.equal(usage.scopedMonitor(monitor, sources, { app: 'chatgpt' }).tokens_per_minute, 2);
  assert.equal(usage.scopedMonitor(monitor, sources, { app: 'chatgpt', sourceId: 'desktop' }).tokens_per_minute, 3);
});

test('daily ranges count calendar days and keep leap-day continuity', () => {
  assert.deepEqual(usage.dateRange('7', new Date('2026-10-29T12:00:00')), { from: '2026-10-23', to: '2026-10-29' });
  const days = usage.fillDays([], '2024-02-28', '2024-03-01');
  assert.deepEqual(days.map(day => day.date), ['2024-02-28', '2024-02-29', '2024-03-01']);
  assert.equal(days[1].requests, null);
  assert.deepEqual(usage.fillDays([], '2026-10-30', '2026-10-29'), []);
});

test('monitor labels use the matching exact or combined rate and preserve unknown timing', () => {
  const monitor = { samples: 3, interval_seconds: 30, tokens_per_minute: 150,
    exact_tokens_per_minute: 100, estimated_tokens_per_minute: 50, output_tokens_per_second: null,
    average_first_token_ms: 1250, timing_samples: 0,
    history: [{ timestamp: 1, tokens_per_minute: 50, exact_tokens_per_minute: 40 }, { timestamp: 2, tokens_per_minute: null }] };
  assert.equal(usage.monitorValues(monitor).rate, 150);
  const real = usage.monitorValues(monitor, false);
  assert.equal(real.rate, 100);
  assert.equal(real.samples, 3);
  assert.equal(real.interval, 30);
  assert.equal(real.firstTokenSeconds, 1.25);
  assert.equal(real.throughput, null);
  assert.deepEqual(real.history, [{ timestamp: 1, rate: 40 }]);
  assert.equal(usage.monitorValues({}).rate, null);
});

test('in-flight background scans retain old charts and complete after a manual refresh while paused', () => {
  const previous = { sources: [source('a', 'tokens', [])], generated_at: 123 };
  const merged = usage.mergeSnapshot(previous, { sources: [], updating: true });
  assert.equal(merged.generated_at, 123);
  assert.equal(merged.sources.length, 1);
  assert.equal(merged.updating, true);
  assert.equal(usage.pollDelay(true, false, true, 0), 2000);
  assert.equal(usage.pollDelay(false, false, true, 30), 0);
  assert.equal(usage.pollDelay(true, true, true, 30), 0);
  assert.equal(usage.pollDelay(true, false, false, 0), 0);
});
