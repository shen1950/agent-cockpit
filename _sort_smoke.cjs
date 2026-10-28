// 无头冒烟：密钥页拖拽排序
// 1) 页面渲染出站点卡与 key 行的手柄  2) 直接调 mock 的 site_reorder/key_reorder 后列表顺序真的变了
const http = require('http');
const crypto = require('crypto');

function get(path) {
  return new Promise((resolve, reject) => {
    http.get({ host: '127.0.0.1', port: 9333, path, agent: false }, (r) => {
      let b = '';
      r.on('data', (c) => (b += c));
      r.on('end', () => resolve(b));
    }).on('error', reject);
  });
}

async function main() {
  // 找 CDP 目标
  const list = JSON.parse(await get('/json/list'));
  let page = list.find((t) => t.type === 'page' && t.webSocketDebuggerUrl);
  if (!page) throw new Error('no page target');

  const ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let seq = 0;
  const pend = new Map();
  ws.onmessage = (ev) => {
    const m = JSON.parse(ev.data);
    if (m.id && pend.has(m.id)) { pend.get(m.id)(m); pend.delete(m.id); }
  };
  function send(method, params = {}) {
    const id = ++seq;
    return new Promise((res) => { pend.set(id, res); ws.send(JSON.stringify({ id, method, params })); });
  }
  async function evalJS(expr) {
    const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true });
    if (r.result && r.result.exceptionDetails) throw new Error(JSON.stringify(r.result.exceptionDetails).slice(0, 400));
    return r.result && r.result.result ? r.result.result.value : undefined;
  }

  // 等应用渲染完
  await send('Page.enable');
  await send('Page.navigate', { url: 'http://127.0.0.1:8899/index.html' });
  for (let i = 0; i < 40; i++) {
    const ready = await evalJS(`typeof S !== 'undefined' && !!document.querySelector('#keysites .site')`);
    if (ready) break;
    await new Promise((r) => setTimeout(r, 250));
  }

  const out = {};
  // 切到密钥页再等它画完
  out.nav = await evalJS(`(async () => {
    S.view = 'keys'; render(); await load();
    await new Promise((r) => setTimeout(r, 600));
    return document.querySelectorAll('#keysites .site').length + ' sites, ' +
      document.querySelectorAll('#keysites .dragdot').length + ' dots';
  })()`);

  out.sids = await evalJS(`[...document.querySelectorAll('#keysites .site')].map(s => s.dataset.sid).join(',')`);
  out.before = await evalJS(`S.data.keys.map(p => p.name).join(' > ')`);

  // 直接调 mock 的 reorder 动作（等价于拖拽落盘那条 invoke）
  out.reorder = await evalJS(`(async () => {
    const order = S.data.keys.map(p => p.id).reverse();
    const r = await invoke('action', { payload: { action: 'site_reorder', order } });
    return r.ok + ' ' + r.msg;
  })()`);
  await evalJS(`load()`);
  await new Promise((r) => setTimeout(r, 500));
  out.after = await evalJS(`S.data.keys.map(p => p.name).join(' > ')`);

  // 站内 key 排序：取第一个有 ≥2 条 key 的站点
  out.keyReorder = await evalJS(`(async () => {
    const p = S.data.keys.find(x => (x.keys||[]).length >= 2);
    if (!p) return 'no-site-with-2-keys';
    const before = p.keys.map(k => k.id).join(',');
    const order = p.keys.map(k => k.id).reverse();
    const r = await invoke('action', { payload: { action: 'key_reorder', site: p.id, order } });
    await load();
    await new Promise(r2=>setTimeout(r2,400));
    const p2 = S.data.keys.find(x => x.id === p.id);
    return r.ok + ' | before=' + before + ' | after=' + p2.keys.map(k => k.id).join(',');
  })()`);

  // 手柄存在性 + draggable 关闭状态
  out.handles = await evalJS(`document.querySelectorAll('#keysites .dragdot').length`);
  out.draggableIdle = await evalJS(`[...document.querySelectorAll('#keysites [draggable]')].map(n=>n.tagName).join(',') || 'none-draggable'`);

  // 端到端：合成 HTML5 拖拽——按下手柄 → dragstart → 在第 3 张卡下半边 dragover+drop
  out.drag = await evalJS(`(async () => {
    S.view = 'keys'; render(); await load();
    await new Promise((r) => setTimeout(r, 400));
    const boxes = [...document.querySelectorAll('#keysites .site')];
    if (boxes.length < 3) return 'need-3-sites';
    const before = S.data.keys.map(p => p.name).join('|');
    const src = boxes[0], dst = boxes[2];
    src.querySelector('.dragdot').dispatchEvent(new MouseEvent('mousedown', { bubbles: true }));
    if (!src.draggable) return 'handle-did-not-enable-draggable';
    const dt = new DataTransfer();
    src.dispatchEvent(new DragEvent('dragstart', { bubbles: true, dataTransfer: dt }));
    const r = dst.getBoundingClientRect();
    dst.dispatchEvent(new DragEvent('dragover', { bubbles: true, dataTransfer: dt,
      clientY: r.top + r.height * 0.75 }));
    dst.dispatchEvent(new DragEvent('drop', { bubbles: true, dataTransfer: dt,
      clientY: r.top + r.height * 0.75 }));
    await new Promise((r2) => setTimeout(r2, 800));
    const after = S.data.keys.map(p => p.name).join('|');
    const idle = src.draggable ? 'still-draggable!' : 'draggable-cleared';
    return 'before=' + before + ' || after=' + after + ' || ' + idle;
  })()`);

  // 站内 key 拖拽：展开一个 ≥2 条 key 的站点，把第 1 行拖到第 2 行下半边
  out.keyDrag = await evalJS(`(async () => {
    S.view = 'keys';
    const p = S.data.keys.find(x => (x.keys || []).length >= 2);
    if (!p) return 'no-site';
    S.keysOpen[p.id] = true; render(); await load();
    await new Promise((r) => setTimeout(r, 400));
    const rows = [...document.querySelectorAll('#site-' + CSS.escape(p.id) + ' .tbl.keys tbody tr')];
    if (rows.length < 2) return 'need-2-rows';
    const before = p.keys.map(k => k.label || k.id).join(',');
    const src = rows[0], dst = rows[1];
    src.querySelector('.dragdot').dispatchEvent(new MouseEvent('mousedown', { bubbles: true }));
    if (!src.draggable) return 'row-handle-did-not-enable-draggable';
    const dt = new DataTransfer();
    src.dispatchEvent(new DragEvent('dragstart', { bubbles: true, dataTransfer: dt }));
    const r = dst.getBoundingClientRect();
    dst.dispatchEvent(new DragEvent('dragover', { bubbles: true, dataTransfer: dt,
      clientY: r.top + r.height * 0.7 }));
    dst.dispatchEvent(new DragEvent('drop', { bubbles: true, dataTransfer: dt,
      clientY: r.top + r.height * 0.7 }));
    await new Promise((r2) => setTimeout(r2, 800));
    const after = S.data.keys.find(x => x.id === p.id).keys.map(k => k.label || k.id).join(',');
    return 'before=' + before + ' || after=' + after;
  })()`);

  console.log(JSON.stringify(out, null, 2));
  ws.close();
  process.exit(0);
}

main().catch((e) => { console.error('FAIL', e); process.exit(1); });
