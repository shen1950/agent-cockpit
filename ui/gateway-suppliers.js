/* Magpie-style provider list: one admission switch per source, settings in its detail view. */
const gatewaySuppliers = (() => {
  const limits = [
    ['context_window', '上下文窗口', ['128K', '200K', '256K', '400K', '512K', '1M']],
    ['max_output_tokens', '最大输出', ['8K', '16K', '32K', '64K', '128K']],
    ['auto_compact_token_limit', '压缩阈值', ['128K', '200K', '272K', '500K', '1M']],
  ];
  const reasoningLevels = [['', '自动'], ['off', '关闭'], ['low', '低'], ['medium', '中'], ['high', '高']];
  const reasoningName = value => (reasoningLevels.find(([id]) => id === value) || ['', '自动'])[1];

  function tokens(raw) {
    const value = String(raw ?? '').trim();
    if (!value) return null;
    const match = /^(\d+(?:\.\d+)?)([km]?)$/i.exec(value);
    const number = match && Number(match[1]) * ({ k: 1000, m: 1000000 }[match[2].toLowerCase()] || 1);
    if (!Number.isInteger(number) || number < 1 || number > 4294967295)
      throw Error('请输入正整数，或使用 128K、1M 这样的写法。');
    return number;
  }

    function create(host, app = {}) {
      const foldKey = 'cockpit-proxy-fold';
      const readFold = () => {
        try { return JSON.parse(localStorage.getItem(foldKey) || '{}').suppliers === true; } catch { return false; }
      };
      const writeFold = folded => {
        try {
          const state = JSON.parse(localStorage.getItem(foldKey) || '{}');
          state.suppliers = folded;
          localStorage.setItem(foldKey, JSON.stringify(state));
        } catch {}
      };
      let folded = readFold();
      let providers = [], selected = localStorage.getItem('cockpit-gateway-supplier') || '', busy = false, providerSignature = '';
    let keyId = '', selectedModel = '', details = [], policies = {}, draft = {};
    let providerEditor = null, capabilityGeneration = 0;
    const message = el('p', { class: 'gateway-suppliers-message', aria: { live: 'polite' } });

    const invoke = async (action, fields = {}) => {
      const response = await window.invoke('action', { payload: { action, ...fields } });
      if (!response?.ok) throw Error(response?.msg || '操作失败');
      return response;
    };
    const client = async (operation, fields = {}) => window.invoke('client_gateway', {
      app: app.id, name: app.current_account || '', op: operation,
      keyId: fields.keyId || keyId, model: fields.model || selectedModel,
      policy: fields.policy, scope: fields.scope, settings: fields.settings,
    });
    const distKeys = () => ((app.data || (globalThis.S?.data || {})).proxy?.dist || [])
      .filter(key => key.enabled !== false);

    function selector(label, choices, value, change) {
      const input = el('select', { class: 'px-select', aria: { label } });
      for (const [id, title] of choices) input.append(el('option', { value: id }, title));
      input.value = value || '';
      input.onchange = () => change(input.value);
      return el('label', { class: 'gateway-supplier-select' }, label, input);
    }

    function readPolicyDraft(rows) {
      const row = rows.find(item => item.id === selectedModel);
      if (!row) return;
      const stored = policies[row.id] || {};
      draft = { ...stored, image: stored.image ? { ...stored.image } : null };
    }

    async function savePolicy() {
      if (busy || !selectedModel) return;
      const policy = {};
      try {
        for (const [key] of limits) {
          const number = tokens(draft[key]);
          if (number !== null) policy[key] = number;
        }
        if (draft.reasoning) policy.reasoning = draft.reasoning;
        if (policy.max_output_tokens > policy.context_window) throw Error('最大输出不能超过上下文窗口。');
        if (draft.image) {
          if (draft.image.mode === 'describe' && !draft.image.model)
            throw Error('请选择同一把密钥下支持原生读图的模型。');
          policy.image = draft.image;
        }
        busy = true; paint();
        await client('save_policy', { model: selectedModel, scope: 'model', policy });
        const catalog = await client('models');
        details = catalog.details || [];
        policies = catalog.policies || {};
        readPolicyDraft(details);
        message.textContent = '已保存。新请求立即使用这些设置。';
      } catch (error) {
        message.textContent = String(error.message || error);
      } finally {
        busy = false; paint();
      }
    }

    function providerEditorPanel() {
      if (!selectedModel) return el('p', { class: 'gateway-supplier-empty' }, '选择模型后可以设置供应商调度和计价。');
      if (!providerEditor) providerEditor = gatewayProvider.create(client);
      return providerEditor.render({ model: selectedModel, scope: 'provider', keyId, models: details });
    }

    function detailPanel(providerRow) {
      const box = el('div', { class: 'gateway-supplier-detail' });
      if (!providerRow.has_token)
        box.append(el('p', { class: 'gateway-supplier-warning' }, '这个来源还没有读到令牌或登录态，启用后网关也不会列出模型。'));
      box.append(message, el('p', { class: 'gateway-hint' }, '先选择接入密钥和模型，再设置这个供应商的能力、调度和计价。'));
      const keyChoices = distKeys().map(key => [key.id, key.name]);
      if (!keyChoices.length) {
        box.append(el('p', {}, '还没有接入密钥。先生成一把密钥，再回来设置供应商。'));
        return box;
      }
      if (!keyChoices.some(([id]) => id === keyId)) keyId = keyChoices.length === 1 ? keyChoices[0][0] : '';
      box.append(selector('接入密钥', [['', '选择接入密钥'], ...keyChoices], keyId, id => {
        keyId = id; details = []; policies = {}; selectedModel = ''; draft = {};
        providerEditor?.clear(); providerEditor = null; loadCapabilities();
      }));
      if (!details.length) {
        box.append(el('p', { class: 'gateway-supplier-empty' },
          keyId ? '正在读取模型目录…' : '选择接入密钥后读取这个供应商的模型。'));
        return box;
      }
      const modelSelector = selector('设置模型', [['', '选择模型'], ...details.map(item => [item.id, item.id])],
        selectedModel, id => {
          selectedModel = id; providerEditor?.clear(); providerEditor = null;
          readPolicyDraft(details); paint();
        });
      if (!selectedModel) {
        box.append(modelSelector, el('p', { class: 'gateway-supplier-empty' }, '选择一个模型，编辑它的能力与来源设置。'));
        return box;
      }
      const modelRow = details.find(item => item.id === selectedModel);
      if (!modelRow) return box;
      const vision = modelRow.native_vision ? '原生读图'
        : modelRow.vision_model ? '由 ' + modelRow.vision_model + ' 代读图片' : '仅文本';
      box.append(el('p', { class: 'gateway-supplier-facts' }, '当前生效　' + [
        vision,
        ...(modelRow.reasoning_effort ? ['推理 ' + reasoningName(modelRow.reasoning_effort)] : []),
        ...limits.filter(([key]) => modelRow[key] > 0)
          .map(([key, name]) => name + ' ' + Number(modelRow[key]).toLocaleString()),
      ].join(' · ')));
      const imageModes = [['', '自动'], ['native', '原生读图'], ['describe', '指定视觉模型代读'], ['text', '仅文本']];
      if (modelRow.native_image_transport === false) imageModes.splice(1, 1);
      const eyes = details.filter(item => item.native_vision && item.id !== modelRow.id);
      const controls = el('div', { class: 'gateway-controls' });
      controls.append(selector('图片输入', imageModes, draft.image?.mode || '', mode => {
        draft.image = mode ? { mode, ...(mode === 'describe' ? { model: eyes[0]?.id || '' } : {}) } : null;
        paint();
      }));
      if (draft.image?.mode === 'describe')
        controls.append(selector('视觉模型', [['', '选择能读图的模型'], ...eyes.map(item => [item.id, item.id])],
          draft.image.model, id => { draft.image.model = id; }));
      box.append(modelSelector, controls,
        el('p', { class: 'gateway-hint' }, '自动优先使用模型自身的图片能力。纯文本模型可由同一把密钥下的视觉模型读取图片，再用描述回答。'));
      for (const [key, title, presets] of limits) {
        const field = el('input', {
          class: 'px-input', type: 'text', size: 10, value: draft[key] ?? '',
          placeholder: '自动', aria: { label: title },
        });
        field.oninput = () => { draft[key] = field.value; };
        const line = el('div', { class: 'gateway-controls' }, el('label', {}, title, field));
        for (const preset of presets)
          line.append(el('button', { class: 'btn sm', onclick: () => { field.value = preset; draft[key] = preset; } }, preset));
        box.append(line);
      }
      box.append(selector('推理思考', reasoningLevels, draft.reasoning || '', value => { draft.reasoning = value; }),
        el('p', { class: 'gateway-hint' }, '低/中/高会作为 reasoning_effort 注入请求；关闭会剥掉客户端自带的推理参数。上游是否支持以各厂商为准。'),
        el('p', { class: 'gateway-hint' }, '留空沿用来源或模型目录。压缩阈值用于 Codex 和 Claude Code，达到或超过上下文窗口时使用整个窗口。'),
        el('button', { class: 'btn primary sm', onclick: savePolicy }, '保存模型设置'),
        el('div', { class: 'gateway-supplier-editor' }, providerEditorPanel()));
      return box;
    }

    async function toggleProvider(row, input) {
      if (busy) { input.checked = row.off === false; return; }
      busy = true; paint();
      try {
        await invoke('proxy_source_toggle', { src: row.src });
        await load(true);
        message.textContent = row.off ? '已启用供应商。' : '已停用供应商。';
        if (selected) paint();
      } catch (error) {
        message.textContent = String(error.message || error);
        paint();
      } finally {
        busy = false;
      }
    }

    function detailView(row) {
      const head = el('div', { class: 'gateway-supplier-head' },
        el('button', { class: 'gateway-supplier-back', onclick: () => {
          selected = ''; localStorage.removeItem('cockpit-gateway-supplier');
          providerEditor?.clear(); providerEditor = null; paint();
        } }, '‹ 返回供应商'),
        el('b', {}, row.name || '供应商'),
        el('span', { class: 'chip' + (row.off ? ' expired' : ' swap') }, row.off ? '已停用' : '已启用'),
        el('span', { class: 'sp' }),
        el('button', { class: 'btn sm', onclick: async event => {
          const button = event.currentTarget;
          if (busy) return;
          const sw = el('input', { type: 'checkbox', class: 'gateway-switch', checked: !row.off });
          button.disabled = true;
          await toggleProvider(row, sw);
          button.disabled = false;
        } }, row.off ? '启用' : '停用'));
      return el('section', { class: 'panel gateway-supplier-page' + (folded ? ' px-folded' : ''), aria: { label: row.name || '供应商详情' } },
        head, detailPanel(row));
    }

    function listView() {
      const section = el('section', { class: 'panel gateway-supplier-page' + (folded ? ' px-folded' : ''), aria: { label: '供应商' } });
      section.append(el('div', { class: 'px-head gateway-supplier-head' },
        el('b', {}, '供应商'), el('span', { class: 'sp' }),
        el('button', { class: 'btn sm', onclick: () => { folded = !folded; writeFold(folded); paint(); } }, folded ? '展开' : '收起'),
        el('button', { class: 'btn sm', onclick: () => load(true) }, '刷新')));
      if (!providers.length) {
        section.append(el('p', { class: 'gateway-supplier-empty' }, '没有可用的供应商。先在账号页或密钥页登记可用来源。'));
        return section;
      }
      for (const row of providers) {
        const sw = el('input', {
          type: 'checkbox', class: 'gateway-switch', checked: !row.off,
          'aria-label': (row.off ? '启用 ' : '停用 ') + (row.name || '供应商'),
        });
        sw.onclick = event => event.stopPropagation();
        sw.onchange = () => toggleProvider(row, sw);
        section.append(el('div', { class: 'gateway-supplier-item' },
          el('button', { class: 'gateway-supplier-row', onclick: () => {
            selected = row.id; localStorage.setItem('cockpit-gateway-supplier', selected);
            providerEditor?.clear(); providerEditor = null; paint();
            if (keyId && !details.length) loadCapabilities();
          } },
            el('span', { class: 'gateway-supplier-label' },
              el('b', {}, row.name || '供应商'),
              el('small', {}, row.app_name || row.site_name || row.kind)),
            sw, el('span', { class: 'gateway-supplier-caret' }, '›'))));
      }
      return section;
    }

    async function loadCapabilities() {
      if (!keyId || !selected) { paint(); return; }
      const generation = ++capabilityGeneration;
      paint();
      try {
        const catalog = await client('models');
        if (generation !== capabilityGeneration) return;
        details = catalog.details || [];
        policies = catalog.policies || {};
        selectedModel = details.some(row => row.id === selectedModel) ? selectedModel : details[0]?.id || '';
        readPolicyDraft(details);
        message.textContent = '';
      } catch (error) {
        if (generation === capabilityGeneration) message.textContent = String(error.message || error);
      } finally {
        if (generation === capabilityGeneration) paint();
      }
    }

    async function load(force = false) {
      if (busy) return;
      busy = true;
      try {
        const response = await invoke('proxy_providers');
        const raw = typeof response.msg === 'string' ? JSON.parse(response.msg) : (response.result || response);
        providers = raw.providers || [];
        const signature = JSON.stringify(providers);
        const changed = force || signature !== providerSignature;
        providerSignature = signature;
        if (!providers.some(row => row.id === selected)) {
          selected = '';
          localStorage.removeItem('cockpit-gateway-supplier');
        }
        if (changed) paint();
      } catch (error) {
        message.textContent = String(error.message || error);
        paint();
      } finally {
        busy = false;
      }
    }

    function paint() {
      if (!host) return;
      const selectedRow = providers.find(row => row.id === selected);
      clear(host);
      if (selectedRow) host.append(detailView(selectedRow));
      else host.append(listView());
    }

    return { get providers() { return providers; }, load, paint };
  }

  return { create };
})();
