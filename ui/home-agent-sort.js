/* 首页卡片与侧栏共用 settings.app_order。事件委托挂在稳定的 home-body 上，
   每帧重新寻找卡片，因此四秒轮询重建 DOM 时也能继续拖动。 */
(() => {
  const host = document.getElementById('home-body');
  if (!host) return;
  let saving = false;
  let drag = null;
  let blockClick = false;
  let orderRevision = 0;
  let confirmedOrder = [];
  let readingOrder = null;

  function applyConfirmedOrder() {
    if (!S.data?.apps) return;
    const positions = new Map(confirmedOrder.map((id, i) => [id, i]));
    S.data.apps.sort((a, b) => (positions.get(a.id) ?? positions.size) - (positions.get(b.id) ?? positions.size));
    if (S.data.settings) S.data.settings.app_order = confirmedOrder.slice();
  }

  // get_state 耗时可能跨过一次排序保存。只保护这类旧请求，下一次正常
  // 刷新仍接受后端顺序；在 render 前修正，避免先闪回旧位置再挪回来。
  const loadState = load;
  load = async function (...args) {
    if (S.busy) return loadState(...args);
    const reading = { revision: orderRevision };
    readingOrder = reading;
    try { return await loadState(...args); }
    finally { if (readingOrder === reading) readingOrder = null; }
  };
  const renderState = render;
  render = function (...args) {
    if (readingOrder && readingOrder.revision < orderRevision) applyConfirmedOrder();
    return renderState(...args);
  };
  // 侧栏拖拽也使用同一保存入口，不能让两个入口各自覆盖对方的顺序。
  applyNavSort = (fromId, toId, after) => saveOrder(fromId, toId, after);

  const cards = () => [...host.querySelectorAll('.hagents .hacard')];
  const apps = () => productEntries(S.data?.apps || []);
  const clearMarks = () => host.querySelectorAll('.ha-drag-source, .ha-drop-before, .ha-drop-after')
    .forEach(node => node.classList.remove('ha-drag-source', 'ha-drop-before', 'ha-drop-after'));

  // 点下去没移动仍是原来的导航；真正拖过之后产生的 click 不应打开账号页。
  document.addEventListener('pointerdown', () => { blockClick = false; }, true);
  document.addEventListener('click', e => {
    if (!blockClick || e.detail === 0) return;
    blockClick = false;
    e.preventDefault();
    e.stopImmediatePropagation();
  }, true);
  host.addEventListener('dragstart', e => {
    if (e.target.closest('.hacard')) e.preventDefault(); // 品牌图标不能变成原生拖图片
  });

  async function saveOrder(fromId, toId, after) {
    if (saving) return;
    const ids = (S.data?.apps || []).map(a => a.id);
    const from = ids.indexOf(fromId);
    if (from < 0 || !ids.includes(toId) || fromId === toId) return;
    ids.splice(from, 1);
    ids.splice(ids.indexOf(toId) + (after ? 1 : 0), 0, fromId);
    if (ids.every((id, i) => id === S.data.apps[i].id)) return;
    saving = true;
    try {
      const result = await invoke('action', { payload: { action: 'app_reorder', app: fromId, ids } });
      if (!result.ok) throw new Error(result.msg || '保存失败');
      confirmedOrder = ids;
      orderRevision += 1;
      applyConfirmedOrder();
      render();
    } catch (error) {
      toast(`排序未保存：${error.message || error}`, false);
      await load();
    } finally {
      saving = false;
    }
  }

  host.addEventListener('pointerdown', e => {
    const source = e.target.closest('.hagents .hacard');
    if (!source || e.button !== 0 || e.pointerType === 'touch' || !e.isPrimary || saving || drag) return;
    const app = apps()[cards().indexOf(source)];
    if (!app) return;
    const scroller = host.closest('.pagebody');
    drag = { id: app.id, pointer: e.pointerId, x: e.clientX, y: e.clientY,
      x0: e.clientX, y0: e.clientY, active: false, target: null, ghost: null, frame: 0 };

    const paint = () => {
      clearMarks();
      drag.target = null;
      const nodes = cards(), visible = apps();
      nodes[visible.findIndex(a => a.id === drag.id)]?.classList.add('ha-drag-source');
      const under = document.elementFromPoint(drag.x, drag.y)?.closest('.hagents .hacard');
      const index = nodes.indexOf(under);
      if (index < 0 || !visible[index] || visible[index].id === drag.id) return;
      const rect = under.getBoundingClientRect();
      const after = drag.x > rect.left + rect.width / 2;
      under.classList.add(after ? 'ha-drop-after' : 'ha-drop-before');
      drag.target = { id: visible[index].id, after };
    };
    const tick = () => {
      if (!drag?.active) return;
      if (S.view !== 'home') { finish(false); return; }
      // 靠近可滚动区域边缘时继续滚动，窄窗口也能移到最后一行。
      const rect = scroller?.getBoundingClientRect();
      if (rect && drag.x >= rect.left && drag.x <= rect.right && drag.y >= rect.top && drag.y <= rect.bottom) {
        const edge = 36;
        const dy = drag.y < rect.top + edge ? -8 : drag.y > rect.bottom - edge ? 8 : 0;
        if (dy) scroller.scrollTop += dy;
      }
      paint();
      drag.frame = requestAnimationFrame(tick);
    };
    const move = ev => {
      if (ev.pointerId !== drag.pointer) return;
      drag.x = ev.clientX; drag.y = ev.clientY;
      if (!drag.active) {
        if (Math.hypot(drag.x - drag.x0, drag.y - drag.y0) < 6) return;
        drag.active = true;
        blockClick = true;
        document.body.classList.add('ha-sorting');
        const ghost = document.createElement('div');
        ghost.className = 'ha-sort-ghost';
        ghost.textContent = app.name;
        ghost.setAttribute('aria-hidden', 'true');
        document.body.appendChild(ghost);
        drag.ghost = ghost;
        drag.frame = requestAnimationFrame(tick);
      }
      ev.preventDefault();
      drag.ghost.style.left = `${Math.max(4, Math.min(drag.x + 14, innerWidth - drag.ghost.offsetWidth - 4))}px`;
      drag.ghost.style.top = `${Math.max(4, drag.y - 36)}px`;
      paint();
    };
    const finish = commit => {
      if (commit && drag.active) paint();
      const ended = drag;
      drag = null;
      cancelAnimationFrame(ended.frame);
      ended.ghost?.remove();
      clearMarks();
      document.body.classList.remove('ha-sorting');
      document.removeEventListener('pointermove', move, true);
      document.removeEventListener('pointerup', up, true);
      document.removeEventListener('pointercancel', cancel, true);
      document.removeEventListener('keydown', key, true);
      window.removeEventListener('blur', blur);
      if (commit && ended.active && ended.target) void saveOrder(ended.id, ended.target.id, ended.target.after);
    };
    const up = ev => {
      if (ev.pointerId !== drag.pointer) return;
      drag.x = ev.clientX; drag.y = ev.clientY;
      finish(true);
    };
    const cancel = ev => { if (ev.pointerId === drag.pointer) finish(false); };
    const blur = () => finish(false);
    const key = ev => {
      if (ev.key === 'Escape') {
        ev.preventDefault(); ev.stopPropagation();
        finish(false);
      }
    };
    document.addEventListener('pointermove', move, true);
    document.addEventListener('pointerup', up, true);
    document.addEventListener('pointercancel', cancel, true);
    document.addEventListener('keydown', key, true);
    window.addEventListener('blur', blur);
  });

  // 给轮询重建的新卡片补上操作提示，不改变原有卡面内容。
  const hint = () => cards().forEach(card => {
    if (!card.title.includes('按住拖动调整顺序')) card.title += '\n按住拖动调整顺序 · Esc 取消';
  });
  new MutationObserver(hint).observe(host, { childList: true, subtree: true });
  hint();
})();
