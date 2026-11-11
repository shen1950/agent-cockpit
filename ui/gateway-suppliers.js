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
    let providers = [], selected = '', busy = false, providerSignature = '';
    let settingsDialog = null;
    let keyId = '', selectedModel = '', details = [], policies = {}, draft = {};
    let providerEditor = null, capabilityGeneration = 0, providerGeneration = 0, loading = false, modalOpen = false;
    let capabilitiesLoaded = false, capabilitiesLoading = false;
    const message = el('p', { class: 'gateway-suppliers-message', aria: { live: 'polite' } });

    const invoke = async (action, fields = {}) => {
      const response = await window.__TAURI__.core.invoke('action', { payload: { action, ...fields } });
      if (!response?.ok) throw Error(response?.msg || '操作失败');
      return response;
    };
    const client = async (operation, fields = {}) => window.__TAURI__.core.invoke('client_gateway', {
      app: app.id, name: app.current_account || '', op: operation,
      keyId: fields.keyId || keyId, model: fields.model || selectedModel,
      policy: fields.policy, scope: fields.scope, settings: fields.settings,
    });
    const distKeys = () => ((typeof S !== 'undefined' && S.data || app.data || {}).proxy?.dist || [])
      .filter(key => key.enabled !== false);
    const unpack = response => typeof response.msg === 'string' ? JSON.parse(response.msg) : (response.result || response);

    function resetCapabilities() {
      capabilityGeneration++;
      details = []; policies = {}; selectedModel = ''; draft = {}; capabilitiesLoaded = false; capabilitiesLoading = false;
      providerEditor?.clear(); providerEditor = null;
    }

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
      if (!providerRow.has_token || providerRow.supported === false)
        box.append(el('p', { class: 'gateway-supplier-warning' }, providerRow.availability_reason || '这个来源尚未就绪，请先完成登录或配置反代地址。'));
      box.append(message, el('p', { class: 'gateway-hint' }, '以下为所选接入密钥可访问的模型。模型能力、调度和计价按模型来源共享，不是当前账号的独立设置。'));
      const keyChoices = distKeys().map(key => [key.id, key.name]);
      if (!keyChoices.length) {
        box.append(el('p', {}, '还没有接入密钥。先生成一把密钥，再回来设置供应商。'));
        return box;
      }
      if (!keyChoices.some(([id]) => id === keyId)) keyId = keyChoices.length === 1 ? keyChoices[0][0] : '';
      box.append(selector('接入密钥', [['', '选择接入密钥'], ...keyChoices], keyId, id => {
        keyId = id; resetCapabilities(); loadCapabilities();
      }));
      if (!details.length) {
        box.append(el('p', { class: 'gateway-supplier-empty' },
          keyId ? (capabilitiesLoaded ? '这把接入密钥当前没有可用模型。' : '正在读取模型目录…') : '选择接入密钥后读取模型。'));
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

    async function refreshProviders() {
      const generation = ++providerGeneration;
      const raw = unpack(await invoke('proxy_providers'));
      if (generation !== providerGeneration) return false;
      providers = raw.providers || [];
      if (typeof S !== 'undefined' && S.data?.proxy) S.data.proxy.providers = providers;
      const signature = JSON.stringify(providers);
      const changed = signature !== providerSignature;
      providerSignature = signature;
      if (selected && !providers.some(row => row.id === selected)) {
        selected = ''; localStorage.removeItem('cockpit-gateway-supplier'); resetCapabilities();
      }
      return changed;
    }

    async function toggleProvider(row) {
      if (busy) return;
      busy = true; providerGeneration++; paint();
      try {
        await invoke('proxy_provider_set_enabled', { id: row.id, enabled: !!row.off });
        await refreshProviders();
        message.textContent = row.off ? '已启用供应商。' : '已停用供应商。';
      } catch (error) {
        message.textContent = String(error.message || error);
      } finally {
        busy = false; paint();
      }
    }

    function detailView(row) {
      const head = el('div', { class: 'gateway-supplier-head' },
        el('b', { id: 'supplier-settings-title' }, row.name || '供应商'),
        el('span', { class: 'chip' + (row.off ? ' expired' : ' swap') }, row.off ? '已停用' : '已启用'),
        el('span', { class: 'sp' }),
        el('button', { class: 'btn sm', disabled: busy, onclick: () => toggleProvider(row) }, row.off ? '启用' : '停用'));
      return el('div', { class: 'gateway-supplier-settings-content' },
        head, detailPanel(row));
    }

    function openSettings(row, trigger) {
      if (busy || modalOpen || settingsDialog) return;
      selected = row.id;
      resetCapabilities();
      const signature = providerSignature;
      const scroller = host.closest('.pagebody');
      const scrollTop = scroller?.scrollTop;
      const close = () => { if (!busy) dialog.close(); };
      const dialog = el('dialog', { class: 'dialog gateway-supplier-settings',
        aria: { labelledby: 'supplier-settings-title' } });
      const footer = el('div', { class: 'gateway-supplier-settings-footer' },
        el('span', { class: 'gateway-hint' }, '修改后请点击对应分组的保存按钮。'),
        el('button', { class: 'btn', type: 'button', onclick: close }, '关闭'));
      dialog.append(detailView(row), footer);
      settingsDialog = dialog;
      dialog.addEventListener('keydown', event => {
        if (event.key === 'Escape') event.stopPropagation();
      });
      dialog.addEventListener('cancel', event => {
        event.preventDefault();
        close();
      });
      dialog.addEventListener('close', () => {
        settingsDialog = null;
        selected = '';
        resetCapabilities();
        dialog.remove();
        if (signature !== providerSignature) paint();
        const target = trigger.isConnected ? trigger : [...host.querySelectorAll('.gateway-supplier-row')]
          .find(button => button.dataset.providerId === row.id);
        target?.focus({ preventScroll: true });
        if (scroller) scroller.scrollTop = scrollTop;
      }, { once: true });
      document.body.append(dialog);
      dialog.showModal();
      footer.querySelector('button').focus({ preventScroll: true });
      if (keyId) loadCapabilities();
    }

    async function addSupplierModal() {
      if (busy || modalOpen) return;
      modalOpen = true;
      const previousFocus = document.activeElement;
      let saving = false, members = { instances: [], keys: [] }, suggestedName = '';
      const status = el('p', { class: 'gateway-suppliers-message', aria: { live: 'polite' } }, '正在读取本机来源…');
      const name = el('input', { id: 'supplier-name', class: 'px-input', type: 'text', maxLength: 120 });
      const type = el('select', { id: 'supplier-type', class: 'px-select' },
        el('option', { value: 'instance' }, 'Agent 账号 / 本地桥'), el('option', { value: 'key' }, 'API Key'));
      const source = el('select', { id: 'supplier-source', class: 'px-select', disabled: true });
      const labelFor = row => row.site_name
        ? row.site_name + (row.label ? ' / ' + row.label : '')
        : row.app_name + ' / ' + row.instance + (row.bridge ? '（本地桥）' : '');
      const rows = () => type.value === 'key' ? members.keys || [] : members.instances || [];
      const choose = () => {
        const row = rows().find(row => row.src === source.value);
        const next = row ? labelFor(row) : '';
        if (!name.value.trim() || name.value === suggestedName) name.value = next;
        suggestedName = next;
        save.disabled = !row || saving;
      };
      const populate = () => {
        source.replaceChildren(el('option', { value: '' }, '请选择来源'));
        let count = 0;
        for (const row of rows()) {
          const existing = providers.find(provider => provider.src === row.src);
          if (existing && !existing.off) continue;
          const available = row.has_token !== false && row.supported !== false;
          const reason = !available ? ' — ' + (row.availability_reason || '登录态或桥尚未就绪')
            : existing ? ' — 已停用，添加后启用' : '';
          source.append(el('option', { value: row.src, disabled: !available }, labelFor(row) + reason));
          count++;
        }
        source.disabled = false;
        status.textContent = count ? '已启用的来源不会重复列出。'
          : '没有待添加的来源。可先在账号页或 API Key 页登记来源，或重新添加已删除的供应商。';
        choose();
      };
      const close = () => {
        if (saving) return;
        overlay.remove(); modalOpen = false;
        if (previousFocus?.isConnected) previousFocus.focus();
        else host.querySelector('button')?.focus();
      };
      const cancel = el('button', { class: 'btn', type: 'button', onclick: close }, '取消');
      const save = el('button', { class: 'btn primary', type: 'button', disabled: true, onclick: async () => {
        if (saving || busy) return;
        if (!name.value.trim()) { status.textContent = '请输入供应商名称。'; name.focus(); return; }
        if (!source.value) { status.textContent = '请选择可用的来源。'; return; }
        saving = true; busy = true; providerGeneration++;
        save.disabled = true; cancel.disabled = true; source.disabled = true; type.disabled = true; name.disabled = true;
        status.textContent = '正在添加…';
        let saved = false;
        try {
          await invoke('proxy_provider_upsert', { item: { src: source.value, name: name.value.trim() } });
          saved = true;
          await refreshProviders();
          message.textContent = '已添加供应商「' + name.value.trim() + '」。';
        } catch (error) {
          status.textContent = String(error.message || error);
          if (saved) message.textContent = '已保存，但刷新列表失败：' + status.textContent;
        } finally {
          saving = false; busy = false;
          save.disabled = false; cancel.disabled = false; source.disabled = false; type.disabled = false; name.disabled = false;
          if (saved) { close(); paint(); } else choose();
        }
      } }, '添加');
      const panel = el('div', { class: 'dialog gateway-supplier-dialog', role: 'dialog', aria: { modal: 'true', labelledby: 'supplier-title' }, tabIndex: -1 },
        el('h3', { id: 'supplier-title' }, '添加供应商'),
        el('div', { class: 'fields' },
          el('label', { htmlFor: 'supplier-type' }, '来源类型'), type,
          el('label', { htmlFor: 'supplier-source' }, '选择来源'), source,
          el('label', { htmlFor: 'supplier-name' }, '供应商名称'), name),
        el('p', { class: 'gateway-hint' }, '来源取自本机已登记的账号、API Key 和本地桥。添加后启用该来源，接入密钥仍按原有授权范围使用。'),
        status, el('div', { class: 'gateway-supplier-actions' }, cancel, save));
      const overlay = el('div', { class: 'modal', id: 'supplier-modal' }, panel);
      overlay.onclick = event => { if (event.target === overlay) close(); };
      overlay.onkeydown = event => {
        if (event.key === 'Escape') { event.stopPropagation(); close(); }
        if (event.key === 'Tab') {
          const controls = [...panel.querySelectorAll('input,select,button')].filter(node => !node.disabled);
          const first = controls[0], last = controls.at(-1);
          if (!first) { event.preventDefault(); panel.focus(); }
          else if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last.focus(); }
          else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first.focus(); }
        }
      };
      type.onchange = populate;
      source.onchange = choose;
      document.body.append(overlay);
      type.focus();
      try {
        members = unpack(await invoke('proxy_members')).members || { instances: [], keys: [] };
        if (overlay.isConnected) populate();
      } catch (error) {
        status.textContent = '读取来源失败：' + String(error.message || error);
      }
    }

    async function confirmDeleteSupplier(row) {
      if (busy) return;
      busy = true;
      try {
        if (!await askConfirm('删除供应商「' + row.name + '」后，该来源将退出反代，原账号和 API Key 保留。之后可通过「添加供应商」恢复。', {
          title: '删除供应商', okText: '删除', danger: true,
        })) return;
        providerGeneration++; paint();
        await invoke('proxy_provider_delete', { id: row.id });
        await refreshProviders();
        message.textContent = '已删除供应商「' + row.name + '」。';
      } catch (error) {
        message.textContent = String(error.message || error);
      } finally {
        busy = false; paint();
      }
    }

    function listView() {
      const section = el('section', { class: 'panel gateway-supplier-page' + (folded ? ' px-folded' : ''), aria: { label: '供应商' } });
      section.append(el('div', { class: 'px-head gateway-supplier-head' },
        el('b', {}, '供应商'), el('span', { class: 'sp' }),
        el('button', { class: 'btn sm', onclick: () => { folded = !folded; writeFold(folded); paint(); } }, folded ? '展开' : '收起'),
        el('button', { class: 'btn sm', disabled: busy, onclick: () => load(true) }, '刷新'),
        el('button', { class: 'btn primary sm', disabled: busy, onclick: addSupplierModal }, '添加供应商')), message.cloneNode(true));
      if (!providers.length) {
        section.append(el('p', { class: 'gateway-supplier-empty' },
          '暂无供应商。点击「添加供应商」，选择本机账号、API Key 或本地桥。'));
        return section;
      }
      for (const row of providers) {
        const sw = el('input', {
          type: 'checkbox', class: 'gateway-switch', checked: !row.off, disabled: busy,
          aria: { label: (row.off ? '启用 ' : '停用 ') + (row.name || '供应商') },
          onchange: () => toggleProvider(row),
        });
        const unavailable = row.supported === false || !row.has_token;
        const item = el('div', { class: 'gateway-supplier-item' });
        item.append(el('button', { class: 'gateway-supplier-row', disabled: busy,
          data: { providerId: row.id }, aria: { haspopup: 'dialog' },
          onclick: event => openSettings(row, event.currentTarget) },
          el('span', { class: 'gateway-supplier-label' },
            el('b', {}, row.name || '供应商'),
            el('small', {}, row.app_name || row.site_name || row.kind),
            unavailable ? el('small', { class: 'gateway-supplier-warning' }, row.availability_reason || '登录态或桥尚未就绪') : null),
          el('span', { class: 'gateway-supplier-caret' }, '›')),
          el('div', { class: 'gateway-supplier-ops' }, sw,
            el('button', { class: 'btn danger sm', disabled: busy, title: '删除供应商，保留原账号和 API Key',
              onclick: () => confirmDeleteSupplier(row) }, '删除')));
        section.append(item);
      }
      return section;
    }

    async function loadCapabilities() {
      if (!keyId || !selected) { paint(); return; }
      if (capabilitiesLoading) return;
      capabilitiesLoading = true;
      const generation = ++capabilityGeneration;
      paint();
      try {
        const catalog = await client('models');
        if (generation !== capabilityGeneration) return;
        capabilitiesLoaded = true;
        details = catalog.details || [];
        policies = catalog.policies || {};
        selectedModel = details.some(row => row.id === selectedModel) ? selectedModel : details[0]?.id || '';
        readPolicyDraft(details);
        message.textContent = '';
      } catch (error) {
        if (generation === capabilityGeneration) { capabilitiesLoaded = true; message.textContent = String(error.message || error); }
      } finally {
        if (generation === capabilityGeneration) { capabilitiesLoading = false; paint(); }
      }
    }

    async function load(force = false) {
      if (busy || loading || settingsDialog) return;
      loading = true;
      try {
        const changed = await refreshProviders();
        if (settingsDialog) return;
        if (changed || force) paint();
        if (selected && keyId && !capabilitiesLoaded && !details.length) loadCapabilities();
      } catch (error) {
        message.textContent = String(error.message || error);
        paint();
      } finally {
        loading = false;
      }
    }

    function paint() {
      if (!host) return;
      if (settingsDialog) {
        const selectedRow = providers.find(row => row.id === selected);
        if (!selectedRow) { settingsDialog.close(); return; }
        const previous = settingsDialog.querySelector('.gateway-supplier-settings-content');
        const scrollTop = previous.querySelector('.gateway-supplier-detail').scrollTop;
        const focusedLabel = settingsDialog.contains(document.activeElement)
          ? document.activeElement.getAttribute('aria-label') : null;
        const content = detailView(selectedRow);
        previous.replaceWith(content);
        if (focusedLabel) [...content.querySelectorAll('[aria-label]')]
          .find(node => node.getAttribute('aria-label') === focusedLabel)?.focus({ preventScroll: true });
        content.querySelector('.gateway-supplier-detail').scrollTop = scrollTop;
        return;
      }
      clear(host);
      host.append(listView());
    }

    return { get providers() { return providers; }, load, paint };
  }

  return { create };
})();
