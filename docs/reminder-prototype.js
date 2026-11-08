/* Browser-only interaction study. No account access, native windows or notifications. */
(() => {
  'use strict';
  const $ = id => document.getElementById(id);
  const desktop = $('desktop'), bubble = $('bubble');
  const status = message => $('status').textContent = message;
  const clamp = (n, lo, hi) => Math.max(lo, Math.min(n, Math.max(lo, hi)));
  const sides = ['left', 'right', 'top', 'bottom'];
  let taskbar = 'bottom', mode = 'both', empty = false, activeDrag = null, bubbleWanted = true, hint = false;
  let keyboardMode = false, pointer = { x: -1, y: -1 };
  document.addEventListener('keydown',event=>{if(event.key==='Tab'||event.key.startsWith('Arrow'))keyboardMode=true},true);
  document.addEventListener('pointerdown',()=>{keyboardMode=false},true);
  document.addEventListener('pointermove',event=>{pointer={x:event.clientX,y:event.clientY}},true);
  const entries = [
    { node: $('panel'), handle: $('panelHandle'), tab: $('edge'), x: 0, y: 0, edge: null, closed: false, pinned: false },
    { node: $('pet'), handle: $('petHandle'), tab: $('petEdge'), x: 0, y: 0, edge: null, closed: false, pinned: false },
  ];
  const [panel, orb] = entries;
  for (const e of entries) {
    const seam = document.createElement('button');
    seam.type = 'button'; seam.className = 'edge-seam'; seam.hidden = true;
    seam.setAttribute('aria-label', e === panel ? '展开临期提醒' : '展开悬浮球');
    desktop.append(seam); e.seam = seam;
  }
  const visible = entry => !entry.dismissed && (entry === panel ? mode !== 'petOnly' : mode !== 'panelOnly');
  function bounds() {
    return { left: taskbar === 'left' ? 38 : 0, top: taskbar === 'top' ? 38 : 0,
      right: desktop.clientWidth - (taskbar === 'right' ? 38 : 0), bottom: desktop.clientHeight - (taskbar === 'bottom' ? 38 : 0) };
  }
  function size(e) { return { w: e.width || e.node.offsetWidth, h: e.height || e.node.offsetHeight }; }
  function nearest(e, force = false) {
    const b = bounds(), { w, h } = size(e);
    const distances = { left: e.x-b.left, right: b.right-e.x-w, top: e.y-b.top, bottom: b.bottom-e.y-h };
    const edge = sides.filter(s => s !== taskbar).sort((a,b) => distances[a]-distances[b])[0];
    return force || distances[edge] <= 20 ? edge : null;
  }
  function move(e, x, y) {
    const b = bounds(), { w,h } = size(e);
    e.x = clamp(x,b.left,b.right-w); e.y = clamp(y,b.top,b.bottom-h);
    e.node.style.left = `${e.x}px`; e.node.style.top = `${e.y}px`;
    e.node.dataset.edge = e.edge || '';
    positionBubble();
  }
  function align(e) {
    const b = bounds(), { w,h } = size(e);
    move(e, e.edge === 'left' ? b.left : e.edge === 'right' ? b.right-w : e.x,
      e.edge === 'top' ? b.top : e.edge === 'bottom' ? b.bottom-h : e.y);
  }
  function placeTab(e) {
    const b = bounds(), vertical = ['left','right'].includes(e.edge), w=vertical?24:86, h=vertical?86:24;
    const s = size(e); e.tab.dataset.edge = e.edge;
    e.tab.style.left = `${e.edge === 'left' ? b.left : e.edge === 'right' ? b.right-w : clamp(e.x+s.w/2-w/2,b.left,b.right-w)}px`;
    e.tab.style.top = `${e.edge === 'top' ? b.top : e.edge === 'bottom' ? b.bottom-h : clamp(e.y+s.h/2-h/2,b.top,b.bottom-h)}px`;
  }
  function placeSeam(e) {
    const b = bounds(), s = size(e), t = 5;
    e.seam.dataset.edge = e.edge;
    const vertical = e.edge === 'left' || e.edge === 'right';
    e.seam.style.width = `${vertical ? t : clamp(s.w, 0, b.right-b.left)}px`;
    e.seam.style.height = `${vertical ? clamp(s.h, 0, b.bottom-b.top) : t}px`;
    e.seam.style.left = `${e.edge === 'left' ? b.left : e.edge === 'right' ? b.right-t : clamp(e.x, b.left, b.right-s.w)}px`;
    e.seam.style.top = `${e.edge === 'top' ? b.top : e.edge === 'bottom' ? b.bottom-t : clamp(e.y, b.top, b.bottom-s.h)}px`;
  }
  function paint(e) {
    // Keep the element rendered while docking so transform can interpolate both ways.
    e.node.hidden = !visible(e);
    const {w,h}=size(e);
    const b = bounds();
    // 提示条关闭时按 TieZ 方案收缝：卡片保持原尺寸滑出，只留 3px 贴在
    // 工作区边缘；开启提示条时整卡滑出，由标签承担展开入口。
    const dx=e.edge==='left'?(hint?-(e.x+w+8):b.left-w+3-e.x):e.edge==='right'?(hint?desktop.clientWidth-e.x+8:b.right-3-e.x):0;
    const dy=e.edge==='top'?(hint?-(e.y+h+8):b.top-h+3-e.y):e.edge==='bottom'?(hint?desktop.clientHeight-e.y+8:b.bottom-3-e.y):0;
    e.node.style.setProperty('--dock-x',`${dx}px`);e.node.style.setProperty('--dock-y',`${dy}px`);
    e.node.classList.toggle('is-docked',e.closed);
    e.node.dataset.docked=String(e.closed);
    e.node.inert=e.closed;
    e.node.setAttribute('aria-hidden',String(!visible(e)||e.closed));
    e.tab.hidden = !visible(e) || !e.closed || !hint;
    e.seam.hidden = !visible(e) || !e.closed || hint;
    if (e.closed && hint) placeTab(e);
    if (e.closed && !hint) placeSeam(e);
    positionBubble();
  }
  function collapse(e, force = false) {
    if (e.pinned || !visible(e) || activeDrag) return;
    clearTimeout(e.timer);
    e.edge = e.edge && e.edge !== taskbar ? e.edge : nearest(e, force);
    if (!e.edge) return;
    if(e.node.contains(document.activeElement))document.activeElement.blur();
    align(e); e.closed = true; paint(e);
  }
  function expand(e) {
    clearTimeout(e.timer); clearTimeout(e.hoverTimer);
    e.closed = false; paint(e); align(e);
  }
  function pointerInside(e) {
    const r=e.node.getBoundingClientRect();
    return pointer.x>=r.left && pointer.x<=r.right && pointer.y>=r.top && pointer.y<=r.bottom;
  }
  function schedule(e, delay = 160) {
    clearTimeout(e.timer);
    if (!e.edge || e.pinned || e.closed || !visible(e)) return;
    e.timer = setTimeout(() => {
      // Mouse focus left by clicking Pin must not hold the window open. Preserve
      // only active keyboard navigation, just as dragging/hover pauses docking.
      if (!pointerInside(e) && !(keyboardMode && e.node.contains(document.activeElement)) && !activeDrag) collapse(e);
    },delay);
  }
  function positionBubble() {
    bubble.hidden = !bubbleWanted || !visible(orb) || orb.closed || activeDrag === orb;
    if (bubble.hidden) return;
    const b=bounds(), {w,h}=size(orb), bw=bubble.offsetWidth,bh=bubble.offsetHeight;
    bubble.style.left=`${clamp(orb.x+w/2-bw/2,b.left+8,b.right-bw-8)}px`;
    bubble.style.top=`${clamp(orb.y-bh-12 >= b.top ? orb.y-bh-12 : orb.y+h+12,b.top+8,b.bottom-bh-8)}px`;
  }
  function bind(e) {
    let drag = null, suppressClick = false;
    e.handle.addEventListener('dragstart', event => event.preventDefault());
    e.handle.addEventListener('pointerdown', event => {
      if (event.button !== 0 || (e === panel && event.target.closest('button'))) return;
      // Cancel the browser's image/text drag before it emits pointercancel.
      event.preventDefault(); clearTimeout(e.timer); clearTimeout(e.hoverTimer);
      e.handle.setPointerCapture(event.pointerId);
      e.width=e.node.offsetWidth; e.height=e.node.offsetHeight;
      drag={id:event.pointerId,x:event.clientX,y:event.clientY,left:e.x,top:e.y,moved:false};
      activeDrag=e; e.edge=null; e.node.classList.add('dragging');
      e.node.classList.remove('is-docked');
    });
    e.handle.addEventListener('pointermove', event => {
      if (!drag || event.pointerId !== drag.id) return;
      const dx=event.clientX-drag.x,dy=event.clientY-drag.y;
      if (Math.hypot(dx,dy)>4) drag.moved=true;
      if (!drag.moved) return;
      move(e,drag.left+dx,drag.top+dy);
      const edge=nearest(e); $('snapGuide').hidden=!edge; $('snapGuide').dataset.edge=edge || '';
    });
    const finish = (event,cancelled=false) => {
      if (!drag || event.pointerId!==drag.id) return;
      const moved=drag.moved; drag=null; activeDrag=null;
      e.node.classList.remove('dragging'); $('snapGuide').hidden=true;
      if (e.handle.hasPointerCapture(event.pointerId)) e.handle.releasePointerCapture(event.pointerId);
      suppressClick=moved;
      if (moved && !cancelled) { e.edge=nearest(e); if(e.edge) { align(e); schedule(e); } }
      positionBubble();
    };
    e.handle.addEventListener('pointerup',event=>finish(event));
    e.handle.addEventListener('pointercancel',event=>finish(event,true));
    e.handle.addEventListener('lostpointercapture',event=>{if(drag)finish(event,true)});
    e.handle.addEventListener('click',event=>{
      if (suppressClick) { suppressClick=false; event.preventDefault(); event.stopPropagation(); return; }
      if(e===orb) {bubbleWanted=!bubbleWanted;positionBubble()}
    });
    e.handle.addEventListener('keydown',event=>{
      if(event.target!==e.handle) return;
      if(event.key==='Escape') {collapse(e,true);(hint?e.tab:e.seam).focus();return}
      const delta={ArrowLeft:[-1,0],ArrowRight:[1,0],ArrowUp:[0,-1],ArrowDown:[0,1]}[event.key];
      if(delta) { event.preventDefault(); clearTimeout(e.timer);e.edge=null;move(e,e.x+delta[0]*(event.shiftKey?32:8),e.y+delta[1]*(event.shiftKey?32:8)); }
    });
    e.node.addEventListener('pointerenter',()=>clearTimeout(e.timer));
    e.node.addEventListener('pointerleave',()=>schedule(e));
    e.node.addEventListener('focusout',()=>schedule(e));
    e.tab.addEventListener('pointerenter',()=>{e.hoverTimer=setTimeout(()=>expand(e),120)});
    e.tab.addEventListener('pointerleave',()=>clearTimeout(e.hoverTimer));
    e.tab.addEventListener('click',event=>{expand(e);if(event.detail===0)e.handle.focus()});
    e.seam.addEventListener('pointerenter',()=>{e.hoverTimer=setTimeout(()=>expand(e),120)});
    e.seam.addEventListener('pointerleave',()=>clearTimeout(e.hoverTimer));
    e.seam.addEventListener('click',event=>{expand(e);if(event.detail===0)e.seam.focus()});
  }
  entries.forEach(bind);
  function layout(reset=false) {
    entries.forEach(e=>{clearTimeout(e.timer);clearTimeout(e.hoverTimer);e.node.hidden=false;e.width=e.node.offsetWidth;e.height=e.node.offsetHeight});
    if(reset) {panel.x=desktop.clientWidth-panel.width-28;panel.y=36;orb.x=70;orb.y=desktop.clientHeight-148;entries.forEach(e=>{e.edge=null;e.closed=false;e.dismissed=false})}
    entries.forEach(e=>{if(e.edge===taskbar){e.edge=null;e.closed=false}align(e);paint(e)});
  }
  function dismiss(e) {
    clearTimeout(e.timer);clearTimeout(e.hoverTimer);e.dismissed=true;e.closed=false;paint(e);
    status(e===panel?'提醒浮窗已关闭。点击顶部“提醒浮窗”可重新打开。':'悬浮球已关闭。点击顶部“圆形悬浮球”可重新打开。');
    $(e===panel?'panelOnly':'petOnly').focus({preventScroll:true});
  }
  $('panelClose').onclick=()=>dismiss(panel);
  $('petClose').onclick=event=>{event.stopPropagation();dismiss(orb)};
  $('pin').onclick=()=>{
    panel.pinned=!panel.pinned;$('pin').setAttribute('aria-pressed',String(panel.pinned));
    $('pin').title=panel.pinned?'取消固定':'固定展开';$('pin').setAttribute('aria-label',$('pin').title);
    if(panel.pinned)clearTimeout(panel.timer);else schedule(panel);
  };
  $('taskbarSide').onchange=event=>{taskbar=event.target.value;$('taskbar').dataset.side=taskbar;layout();status('模拟任务栏已移动；对应一边禁止吸附，原来停在该边的入口已展开。')};
  $('hint').onchange=event=>{
    hint=event.target.checked;
    entries.forEach(e=>{if(e.closed)paint(e)});
    status(hint?'贴边提示条已开启：收起后边缘显示提示标签。':'贴边提示条已关闭：收起后只留细缝，靠近边缘悬停即可展开。');
  };
  for(const id of ['both','panelOnly','petOnly'])$(id).onclick=()=>{mode=id;['both','panelOnly','petOnly'].forEach(x=>$(x).classList.toggle('active',x===id));entries.forEach(e=>{if(id==='both'||id===(e===panel?'panelOnly':'petOnly')){e.dismissed=false;e.closed=false}paint(e)})};
  $('petOpen').onclick=()=>{bubbleWanted=false;mode='both';$('both').click();expand(panel)};
  $('dismiss').onclick=()=>{bubbleWanted=false;positionBubble()};
  $('later').onclick=()=>{bubbleWanted=false;positionBubble();status('演示：将在 1 小时后再次提醒，两个入口共享稍后时间。')};
  $('mute').onclick=()=>{bubbleWanted=false;positionBubble();status('演示：今天保持安静，浮窗中的额度仍继续显示。')};
  $('items').addEventListener('click',event=>{if(event.target.closest('.go'))status('演示：正式版会打开对应账号详情，由你选择下一步操作。')});
  const content=$('items').innerHTML;
  $('empty').onclick=()=>{empty=!empty;$('items').innerHTML=empty?'<article class="item"><b>暂时没有即将到期的积分</b><p>有临期资源时会在这里显示。</p></article>':content;$('summary').textContent=`今天 / 24 小时内 · ${empty?0:2} 个账号`;document.querySelectorAll('.badge,.tab-count').forEach(e=>e.textContent=empty?'0':'2');$('bubbleText').textContent=empty?'暂时没有即将到期的积分。':'工作账号还有 320 积分，预计 2 小时后到期。';bubbleWanted=!empty;layout()};
  $('theme').onclick=()=>{const dark=document.body.classList.toggle('night');$('panelHandle').querySelector('img').src=`../ui/brand-${dark?'dark':'light'}.png`};
  $('reset').onclick=()=>{bubbleWanted=true;layout(true);status('位置已重置。拖动标题栏或悬浮球，试试一次跨越整个模拟桌面。')};
  const reduced=matchMedia('(prefers-reduced-motion: reduce)');
  desktop.addEventListener('pointermove',event=>{
    if(reduced.matches || orb.closed || !visible(orb)) return;
    const rect=orb.node.getBoundingClientRect(),x=clamp((event.clientX-rect.x-rect.width/2)/260,-1,1),y=clamp((event.clientY-rect.y-rect.height/2)/260,-1,1);
    orb.node.style.setProperty('--tilt-x',`${x*13}deg`);orb.node.style.setProperty('--tilt-y',`${-y*13}deg`);
    orb.node.style.setProperty('--light-x',`${50+x*25}%`);orb.node.style.setProperty('--light-y',`${50+y*25}%`);
  });
  desktop.addEventListener('pointerleave',()=>{orb.node.style.setProperty('--tilt-x','0deg');orb.node.style.setProperty('--tilt-y','0deg')});
  new ResizeObserver(()=>{if(!activeDrag)layout()}).observe(desktop);
  layout(true);
})();
