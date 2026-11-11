/* Stateful browser regression for supplier CRUD. Run only through agsync.
 * Uses the bundled mock; no real account, credential, gateway or model request.
 */
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const { chromium } = require('playwright');

process.env.NO_PROXY = process.env.no_proxy = '127.0.0.1,localhost';
const root = path.resolve(__dirname, '../ui');
const evidence = path.resolve(__dirname, '../.agent-sync/codex-supplier-audit-20261108-163813-5727-evidence');
const mime = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.svg': 'image/svg+xml', '.png': 'image/png', '.jpg': 'image/jpeg' };
const server = http.createServer((req, res) => {
  const pathname = decodeURIComponent(new URL(req.url, 'http://localhost').pathname);
  if (pathname === '/favicon.ico') { res.writeHead(204).end(); return; }
  const file = path.resolve(root, '.' + (pathname === '/' ? '/index.html' : pathname));
  if (!file.startsWith(root + path.sep)) { res.writeHead(403).end(); return; }
  fs.readFile(file, (error, bytes) => {
    if (error) { res.writeHead(404).end(); return; }
    res.setHeader('Content-Type', mime[path.extname(file)] || 'application/octet-stream');
    res.end(bytes);
  });
});

async function exercise(browser, viewport) {
  const page = await browser.newPage({ viewport });
  page.setDefaultTimeout(15000);
  const errors = [];
  page.on('pageerror', error => errors.push(error.message));
  page.on('console', msg => { if (msg.type() === 'error') errors.push(msg.text()); });
  const label = viewport.width + 'x' + viewport.height;
  const panel = page.locator('#proxy-suppliers');
  const item = name => panel.locator('.gateway-supplier-item').filter({ has: page.getByText(name, { exact: true }) });
  const snapshot = () => page.evaluate(() => window.supplierBaseInvoke('get_state'));
  const calls = action => page.evaluate(action => window.supplierCalls.filter(call => call.payload?.action === action), action);
  const waitMissing = src => page.waitForFunction(src => !window.supplierPage.providers.some(row => row.src === src), src);
  const waitVisible = src => page.waitForFunction(src => window.supplierPage.providers.some(row => row.src === src), src);
  const openAdd = async () => {
    await panel.getByRole('button', { name: '添加供应商', exact: true }).click();
    await page.locator('#supplier-modal').waitFor();
    await page.waitForFunction(() => !document.getElementById('supplier-source').disabled);
  };
  const remove = async (row, confirm = true) => {
    await item(row.name).getByRole('button', { name: '删除', exact: true }).click();
    await page.locator('#ask').waitFor({ state: 'visible' });
    await page.locator(confirm ? '#ask-ok' : '#ask-cancel').click();
    await page.locator('#ask').waitFor({ state: 'hidden' });
    if (confirm) await waitMissing(row.src);
  };

  try {
    await page.addInitScript(() => {
      localStorage.setItem('cockpit-toured', '1');
      // Creating a demonstration distribution key should not overwrite the user's clipboard.
      Object.defineProperty(navigator, 'clipboard', { value: { writeText: async () => {} } });
    });
    await page.goto('http://127.0.0.1:' + server.address().port + '/', { waitUntil: 'domcontentloaded' });
    await page.waitForFunction(() => typeof S !== 'undefined' && S.data);
    await page.evaluate(() => {
      // Tauri exposes __TAURI__.core.invoke. The mock-only shortcut masked
      // a native failure where an otherwise functional list stayed empty.
      window.supplierBaseInvoke = window.__TAURI__.core.invoke;
      delete window.invoke;
      window.supplierCalls = [];
      window.supplierFailAction = '';
      window.supplierHoldAction = '';
      const intercepted = async (command, args = {}) => {
        window.supplierCalls.push({ command, ...structuredClone(args) });
        if (command === 'action' && args.payload?.action === window.supplierFailAction) {
          window.supplierFailAction = '';
          return { ok: false, msg: '供应商回归：模拟保存失败' };
        }
        if (command === 'action' && args.payload?.action === window.supplierHoldAction) {
          window.supplierHoldAction = '';
          await new Promise(resolve => { window.supplierRelease = resolve; });
        }
        return window.supplierBaseInvoke(command, args);
      };
      window.__TAURI__.core.invoke = intercepted;
      invoke = intercepted;
      S.view = 'proxy'; render();
    });
    await panel.locator('.gateway-supplier-item').first().waitFor();
    await page.locator('#proxy-nav [data-sec="suppliers"]').click();
    const providers = await page.evaluate(() => window.supplierPage.providers);
    const agent = providers.find(row => row.kind === 'instance' && row.has_token && row.supported !== false && !row.off);
    const key = providers.find(row => row.kind === 'key' && row.has_token && row.supported !== false && !row.off);
    const unavailable = providers.find(row => !row.has_token || row.supported === false);
    assert.ok(agent && key && unavailable, 'fixture must exercise active Agent, API Key and unavailable sources');
    assert.equal(await panel.locator('.gateway-supplier-item').count(), providers.length, 'every supplier row must be mounted');
    const before = await snapshot();

    const layout = await panel.locator('.gateway-supplier-item').evaluateAll(rows => rows.map(row => {
      const body = row.querySelector('.gateway-supplier-row').getBoundingClientRect();
      const ops = row.querySelector('.gateway-supplier-ops').getBoundingClientRect();
      const label = row.querySelector('.gateway-supplier-label').getBoundingClientRect();
      return { separated: body.right <= ops.left + 1, labelContained: label.right <= body.right + 1, width: row.scrollWidth <= row.clientWidth + 1 };
    }));
    assert.ok(layout.every(row => row.separated && row.labelContained && row.width), 'supplier labels and actions must not overlap');
    await page.screenshot({ path: path.join(evidence, 'supplier-list-' + label + '.png') });

    // A fresh installation with existing credentials must allow adding a source
    // which has never previously belonged to a gateway pool (the local bridge fixture).
    const candidate = before.proxy.members.instances.find(row => row.bridge && row.has_token &&
      !providers.some(provider => provider.src === row.src));
    assert.ok(candidate, 'fixture must include an available local bridge awaiting registration');
    await openAdd();
    await page.locator('#supplier-source').selectOption(candidate.src);
    await page.locator('#supplier-name').fill('首个本地桥 ' + label);
    await page.locator('#supplier-modal').getByRole('button', { name: '添加', exact: true }).click();
    await page.locator('#supplier-modal').waitFor({ state: 'detached' });
    await waitVisible(candidate.src);
    const registered = (await snapshot()).proxy.providers.find(row => row.src === candidate.src);
    assert.equal(registered.name, '首个本地桥 ' + label);
    assert.equal(registered.off, false);
    assert.equal((await snapshot()).proxy.pools.filter(p => p.id === registered.id && p.members[0]?.src === candidate.src).length, 1);

    // Cancelling is side effect free. Disabling must repaint immediately.
    await remove(agent, false);
    assert.equal((await calls('proxy_provider_delete')).length, 0);
    assert.equal(await item(agent.name).count(), 1);
    await item(agent.name).getByRole('checkbox').uncheck();
    await page.waitForFunction(src => window.supplierPage.providers.find(row => row.src === src)?.off === true, agent.src);
    assert.equal(await item(agent.name).getByRole('checkbox').isChecked(), false);
    await remove(agent);
    assert.ok((await snapshot()).proxy.sources_off.includes(agent.src), 'deleting a disabled source must never enable it');
    await panel.getByRole('button', { name: '刷新', exact: true }).click();
    await page.evaluate(() => window.supplierPage.load(true));
    assert.equal(await item(agent.name).count(), 0, 'deleted supplier must stay absent after refresh');

    // Also delete an enabled key and an unsupported source to cover both paths.
    await remove(key);
    await remove(unavailable);
    const deleted = await snapshot();
    assert.ok(deleted.proxy.sources_off.includes(key.src), 'deleting an enabled source must disable it');
    assert.deepEqual(deleted.proxy.dist, before.proxy.dist, 'removal must preserve every existing key authorization');
    assert.deepEqual(deleted.keys, before.keys, 'removal must preserve the API key ledger');
    assert.deepEqual(
      [...deleted.proxy.members.instances, ...deleted.proxy.members.keys].map(row => row.src).sort(),
      [...before.proxy.members.instances, ...before.proxy.members.keys].map(row => row.src).sort(),
      'removal must retain original accounts and keys as addable sources'
    );

    // A real backend error must be visible without navigating into a detail view.
    const other = (await page.evaluate(() => window.supplierPage.providers)).find(row => row.has_token && row.supported !== false);
    await page.evaluate(() => { window.supplierFailAction = 'proxy_provider_delete'; });
    await item(other.name).getByRole('button', { name: '删除', exact: true }).click();
    await page.locator('#ask-ok').click();
    await panel.getByText('供应商回归：模拟保存失败', { exact: true }).waitFor();
    assert.equal(await item(other.name).count(), 1, 'failed deletion must leave supplier visible');

    await openAdd();
    const dialog = page.locator('#supplier-modal');
    const sources = page.locator('#supplier-source');
    assert.ok(await sources.locator('option').evaluateAll(options => options.every(option => !option.value || !option.value.startsWith('key:'))), 'Agent selector must exclude API keys');
    if (unavailable.kind === 'instance') {
      assert.equal(await sources.locator('option').evaluateAll((options, src) => options.find(option => option.value === src)?.disabled, unavailable.src), true, 'unavailable sources must be disabled');
    }
    await sources.selectOption(agent.src);
    assert.ok((await page.locator('#supplier-name').inputValue()).trim());
    await page.locator('#supplier-type').selectOption('key');
    assert.ok(await sources.locator('option').evaluateAll(options => options.every(option => !option.value || option.value.startsWith('key:'))), 'API selector must exclude Agent sources');
    await sources.selectOption(key.src);
    await page.locator('#supplier-type').selectOption('instance');
    await sources.selectOption(agent.src);
    const customName = '回归供应商 · ' + label;
    await page.locator('#supplier-name').fill(customName);
    const bounds = await dialog.locator('.dialog').boundingBox();
    assert.ok(bounds.x >= 0 && bounds.y >= 0 && bounds.x + bounds.width <= viewport.width + 1 && bounds.y + bounds.height <= viewport.height + 1, 'dialog must fit viewport');
    await page.screenshot({ path: path.join(evidence, 'supplier-add-' + label + '.png') });

    await page.evaluate(() => { window.supplierFailAction = 'proxy_provider_upsert'; });
    await dialog.getByRole('button', { name: '添加', exact: true }).click();
    await dialog.getByText('供应商回归：模拟保存失败', { exact: true }).waitFor();
    assert.equal(await dialog.isVisible(), true, 'failed create must keep fields and error visible');
    assert.equal(await page.locator('#supplier-name').inputValue(), customName);

    // Hold the request so two clicks actually overlap, then release it.
    const createsBefore = (await calls('proxy_provider_upsert')).length;
    await page.evaluate(() => { window.supplierHoldAction = 'proxy_provider_upsert'; });
    await dialog.getByRole('button', { name: '添加', exact: true }).evaluate(button => { button.click(); button.click(); });
    await page.waitForFunction(() => typeof window.supplierRelease === 'function');
    assert.equal((await calls('proxy_provider_upsert')).length, createsBefore + 1, 'double click must issue only one create');
    assert.equal(await dialog.getByRole('button', { name: '添加', exact: true }).isDisabled(), true);
    await page.evaluate(() => { window.supplierRelease(); window.supplierRelease = null; });
    await dialog.waitFor({ state: 'detached' });
    await waitVisible(agent.src);
    const restored = await page.evaluate(src => window.supplierPage.providers.find(row => row.src === src), agent.src);
    assert.equal(restored.name, customName);
    assert.equal(restored.off, false);
    assert.equal(restored.id, agent.id, 'restoration must reuse stable supplier ID');
    const create = (await calls('proxy_provider_upsert')).at(-1);
    assert.deepEqual(create.payload.item, { src: agent.src, name: customName }, 'create must use the provider contract, never pool string members');
    assert.equal((await calls('proxy_pool_upsert')).length, 0);
    assert.equal((await calls('proxy_pool_delete')).length, 0);
    await page.evaluate(() => window.supplierPage.load(true));
    assert.equal(await item(customName).count(), 1);

    // Restore the API key through its own selector.
    await openAdd();
    await page.locator('#supplier-type').selectOption('key');
    await page.locator('#supplier-source').selectOption(key.src);
    const keyName = '回归 API Key · ' + label;
    await page.locator('#supplier-name').fill(keyName);
    await page.locator('#supplier-modal').getByRole('button', { name: '添加', exact: true }).click();
    await page.locator('#supplier-modal').waitFor({ state: 'detached' });
    await waitVisible(key.src);
    assert.equal(await item(keyName).count(), 1);
    assert.deepEqual((await snapshot()).proxy.dist, before.proxy.dist, 'restoring a supplier must not broaden existing authorization');

    // Admission UI consumes stable provider IDs; editing old pool grants preserves them.
    await page.evaluate(async () => { S.data = await window.supplierBaseInvoke('get_state'); });
    await page.evaluate(() => openDistModal(S.data.proxy.dist[0]));
    const oldScope = before.proxy.dist[0].pools;
    assert.deepEqual(await page.evaluate(() => distEditing.pools), oldScope, 'opening legacy key must keep its pool grants');
    await page.locator('#gd-save').click();
    await page.locator('#gdmodal').waitFor({ state: 'hidden' });
    const updatedDist = (await calls('proxy_dist_update')).at(-1);
    assert.deepEqual(updatedDist.payload.item.pools, oldScope, 'saving legacy key must preserve authorization');

    await page.evaluate(async () => {
      await window.supplierBaseInvoke('set_settings', { patch: { legal: { proxyAccepted: true } } });
      S.data = await window.supplierBaseInvoke('get_state');
      openDistModal(null);
    });
    const admission = page.locator('#gdmodal');
    const admissionNames = await admission.locator('.gpmem-name').allInnerTexts();
    assert.ok(admissionNames.includes(customName), 'newly named provider must appear in key admission');
    assert.ok(!admissionNames.includes(unavailable.name), 'removed provider must be absent from key admission');
    for (const cb of await admission.locator('.gpmem-cb').all()) await cb.uncheck();
    const admissionRow = admission.locator('.gpmem').filter({ has: page.getByText(customName, { exact: true }) });
    await admissionRow.locator('.gpmem-cb').check();
    await page.locator('#gd-name').fill('供应商回归密钥 ' + label);
    await page.locator('#gd-save').click();
    await page.waitForFunction(() => window.supplierCalls.some(call => call.payload?.action === 'proxy_dist_create'));
    assert.deepEqual((await calls('proxy_dist_create')).at(-1).payload.item.pools, [restored.id], 'new authorization must contain stable provider IDs');
    await admission.getByRole('button', { name: '完成', exact: true }).click();
    await admission.waitFor({ state: 'hidden' });

    // Retain existing model/settings integration coverage.
    const trigger = item(customName).locator('.gateway-supplier-row');
    await trigger.scrollIntoViewIfNeeded();
    const listBounds = await panel.boundingBox();
    const listScroll = await panel.evaluate(node => node.closest('.pagebody').scrollTop);
    const rowCount = await panel.locator('.gateway-supplier-item').count();
    await trigger.click();
    const settings = page.locator('dialog.gateway-supplier-settings');
    await settings.waitFor({ state: 'visible' });
    const settingsBounds = await settings.boundingBox();
    assert.deepEqual(await panel.boundingBox(), listBounds, 'opening settings must preserve list geometry');
    assert.equal(await panel.locator('.gateway-supplier-item').count(), rowCount, 'all suppliers must remain mounted behind settings');
    assert.equal(await panel.evaluate(node => node.closest('.pagebody').scrollTop), listScroll);
    assert.ok(settingsBounds.x >= 0 && settingsBounds.y >= 0 && settingsBounds.x + settingsBounds.width <= viewport.width && settingsBounds.y + settingsBounds.height <= viewport.height);
    const detail = page.locator('.gateway-supplier-detail');
    await detail.getByLabel('接入密钥', { exact: true }).selectOption({ index: 1 });
    await detail.getByLabel('设置模型', { exact: true }).waitFor();
    assert.equal(await detail.getByLabel('设置模型', { exact: true }).locator('option').count(), 3);
    for (const label of ['图片输入', '上下文窗口', '最大输出', '压缩阈值', '推理思考', '每个账号的并发上限', '价格倍率']) {
      assert.ok(await detail.getByLabel(label, { exact: true }).isVisible(), label + ' must be present');
    }
    await detail.getByLabel('上下文窗口', { exact: true }).fill('1M');
    const focusedInput = await detail.getByLabel('上下文窗口', { exact: true }).elementHandle();
    await page.evaluate(() => window.supplierPage.load(true));
    assert.equal(await focusedInput.evaluate(node => node === document.activeElement && node.isConnected), true, 'refresh must preserve focused draft input');
    assert.equal(await detail.getByLabel('上下文窗口', { exact: true }).inputValue(), '1M');
    assert.deepEqual(await settings.boundingBox(), settingsBounds, 'model loading must not resize the settings dialog');
    await detail.getByRole('button', { name: '保存模型设置', exact: true }).click();
    await page.waitForFunction(async () => (await window.supplierBaseInvoke('get_state')).proxy.mockPolicies?.['mock/vision']?.context_window === 1000000);
    await page.screenshot({ path: path.join(evidence, 'supplier-detail-' + label + '.png') });
    await settings.getByRole('button', { name: '关闭', exact: true }).click();
    await settings.waitFor({ state: 'detached' });
    assert.equal(await trigger.evaluate(node => document.activeElement === node), true, 'closing must restore focus to the original supplier');
    assert.equal(await panel.evaluate(node => node.closest('.pagebody').scrollTop), listScroll, 'closing must restore list scroll');
    await trigger.click();
    await settings.waitFor({ state: 'visible' });
    await page.keyboard.press('Escape');
    await settings.waitFor({ state: 'detached' });
    assert.equal(await trigger.evaluate(node => document.activeElement === node), true);
    assert.deepEqual(errors, [], 'no page or console errors: ' + errors.join(' | '));
    console.log(JSON.stringify({ result: 'PASS', viewport, checks: 'CRUD, admission scope, unavailable sources, errors, double submit, model settings' }));
  } finally {
    await page.close();
  }
}

(async () => {
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  fs.mkdirSync(evidence, { recursive: true });
  let browser;
  try {
    browser = await chromium.launch({ channel: 'chrome', headless: true });
    for (const viewport of [{ width: 940, height: 620 }, { width: 1320, height: 860 }]) await exercise(browser, viewport);
    console.log(JSON.stringify({ result: 'PASS', evidence }));
  } finally {
    await browser?.close();
    await new Promise(resolve => server.close(resolve));
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
