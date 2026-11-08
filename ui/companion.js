'use strict';
const invoke = (op, p = {}) => window.__TAURI__.core.invoke('desktop_companion', {op, p});
const $ = id => document.getElementById(id);
const key = 'cockpit-companion-settings-v1';
const requestedKind = new URLSearchParams(location.search).get('kind');
const kind = ['orb','pet','panel'].includes(requestedKind) ? requestedKind : 'panel';
const positionKey = 'cockpit-companion-position-' + kind;
const newId = () => globalThis.crypto?.randomUUID?.() || Date.now().toString(36)+'-'+Math.random().toString(36).slice(2);
const instanceId = newId();
const leaseKey = 'cockpit-companion-notifier-lease';
function leader() {
  const nowMs=Date.now();
  let owner;
  try { owner=JSON.parse(localStorage.getItem(leaseKey)||'null'); } catch (_) {}
  if(!owner || owner.until<nowMs || owner.id===instanceId) {
    localStorage.setItem(leaseKey,JSON.stringify({id:instanceId,until:nowMs+16000}));
    try {return JSON.parse(localStorage.getItem(leaseKey)).id===instanceId} catch (_) {return false}
  }
  return false;
}
function releaseLeader() {
  try {if(JSON.parse(localStorage.getItem(leaseKey)||'null')?.id===instanceId)localStorage.removeItem(leaseKey)} catch (_) {}
}
window.addEventListener('beforeunload',releaseLeader);
const defaults = {appearance:'orb', theme:'system', voice:false, focus:false, pin:false, edge_hint:false, enabled:true, todos:[], seen:{}};
let stored;
try { stored = JSON.parse(localStorage.getItem(key) || '{}'); } catch (_) { stored = {}; }
const settings = {...defaults, ...stored, todos:Array.isArray(stored.todos) ? stored.todos : [], seen:stored.seen || {}};
let view='ball', selected='alerts', anchor=null, edge='', feed={events:[],recent:[]}, alerts=[], previous=null;
let started=false, pollBusy=false, quotaBusy=false, currentNotice='', collapseTimer=null, lastTrigger=0;
const enabledEvents = new Set(['choice','complete']);
const save = () => localStorage.setItem(key,JSON.stringify(settings));
window.addEventListener('storage',event=>{
  if(event.key!==key || !event.newValue)return;
  try {Object.assign(settings,JSON.parse(event.newValue));theme();render()} catch (_) {}
});
const now = () => Date.now()/1000;
function element(tag, className, text) {
  const item=document.createElement(tag);
  if(className)item.className=className;
  if(text != null)item.textContent=text;
  return item;
}
function action(label, click, cls='') {
  const b=element('button',cls,label);b.type='button';b.addEventListener('click',click);return b;
}
function notice(text) {
  currentNotice=text;$('notice').textContent=text;$('notice').hidden=!text;
}
function theme() {
  const dark=settings.theme==='dark'||(settings.theme==='system'&&matchMedia('(prefers-color-scheme:dark)').matches);
  document.documentElement.dataset.theme=dark?'dark':'light';
  $('compact').classList.toggle('pet',kind==='pet');
}
function statusCount() {
  return feed.events.filter(e=>e.kind==='choice' || (e.kind==='complete' && e.at>now()-300)).length + alerts.filter(a=>a.days<=1).length + settings.todos.filter(t=>!t.done&&t.at<=Date.now()).length;
}
function badge() {
  const n=statusCount();$('badge').hidden=!n;$('badge').textContent=n>99?'99+':String(n);
  $('tab-count').textContent=n?String(n):'';$('count').textContent=n?'· '+n:'';
}
async function layout(mode, initial=false) {
  const p={mode,initial};
  if(mode==='tab')p.hint=!!settings.edge_hint;
  if(anchor){p.x=anchor.x;p.y=anchor.y}
  const result=await invoke('layout',p);
  edge=result.edge||'';
  if(mode!=='tab') {anchor={x:result.x,y:result.y};localStorage.setItem(positionKey,JSON.stringify(anchor));}
  view=mode;
  $('compact').hidden=kind==='panel';$('edge-tab').hidden=kind!=='panel'||mode!=='tab';$('panel').hidden=kind!=='panel'||mode==='tab';
}
async function expand() {
  clearTimeout(collapseTimer);
  if(kind!=='panel'){await invoke('show',{kind:'panel'});return}
  await layout('panel');render();
}
async function collapse() {
  if(kind!=='panel')return;
  const result=await invoke('layout',{mode:'panel',...(anchor||{})});
  edge=result.edge||'';anchor={x:result.x,y:result.y};localStorage.setItem(positionKey,JSON.stringify(anchor));
  if(edge&&!settings.pin)await layout('tab');
  else notice('将提醒卡片拖到屏幕边缘后可收起。');
}
function scheduleCollapse() {
  clearTimeout(collapseTimer);
  if(kind==='panel'&&view==='panel'&&!settings.pin&&edge) collapseTimer=setTimeout(()=>{if(!settings.pin)collapse().catch(e=>notice(String(e)))},1400);
}
function section(title){return element('div','section-title',title)}
function addEmpty(container,text){container.append(element('p','empty',text))}
function nav(payload){return invoke('navigate',payload).catch(e=>notice(String(e)))}
function showEvent(e){
  const card=element('article','item'), title=element('strong');
  title.append(element('span','pill',e.kind==='choice'?'等待选择':'本轮结束'),document.createTextNode(e.label+' · '+(e.title||'会话')));
  card.append(title,element('p','',e.summary||''),element('p','small',new Date(e.at*1000).toLocaleString()));
  card.append(action('查看会话',()=>nav({view:'sessions',sid:e.sid})));
  if(e.kind==='choice'&&e.can_focus)card.append(action('唤起应用',async()=>{try{const r=await invoke('open',{sid:e.sid});notice(r.text||'已唤起应用')}catch(err){notice(String(err))}}));
  return card;
}
function showQuota(a){
  const card=element('article','item');card.append(element('strong','',a.app+' · '+a.name));
  const quantity=a.exp_left != null?a.exp_left:a.left;
  card.append(element('p','',(quantity == null?'剩余数量未知':quantity+' '+(a.unit||'积分'))+' · '+(a.days===0?'今天到期':a.days===1?'明天到期':a.expires||'')));
  if(a.label)card.append(element('p','small',a.label));
  card.append(action('查看账号',()=>nav({view:'account',app:a.app_id,name:a.name})));
  return card;
}
function showRecent(s){
  const card=element('article','item');card.append(element('strong','',s.title||'无标题会话'));
  card.append(element('p','small',[s.label,s.project_name,s.ended?new Date(s.ended*1000).toLocaleString():''].filter(Boolean).join(' · ')));
  card.append(action('查看会话',()=>nav({view:'sessions',sid:s.sid})));return card;
}
function renderTodos(main){
  const form=element('form','todo-form'),text=document.createElement('input'),date=document.createElement('input');
  text.placeholder='提醒事项';text.maxLength=160;text.required=true;date.type='datetime-local';date.required=true;
  const add=element('button','','添加提醒');add.type='submit';form.append(text,date,add);
  form.onsubmit=e=>{e.preventDefault();const at=new Date(date.value).getTime();if(!Number.isFinite(at)||at<=Date.now()){notice('请选择未来的提醒时间');return}
    settings.todos.push({id:newId(),text:text.value.trim(),at,done:false,notified:false});save();notice('提醒已添加');render()};
  main.append(form);
  if(!settings.todos.length)addEmpty(main,'还没有定时提醒。');
  [...settings.todos].sort((a,b)=>a.at-b.at).forEach(t=>{
    const row=element('article','item');row.append(element('strong','',t.text),element('p','small',new Date(t.at).toLocaleString()+(t.done?' · 已完成':'')));
    row.append(action(t.done?'重新启用':'完成',()=>{t.done=!t.done;save();render();badge()}),action('删除',()=>{settings.todos=settings.todos.filter(x=>x.id!==t.id);save();render();badge()}));main.append(row);
  });
}
function render(){
  $('pin').classList.toggle('on',!!settings.pin);
  document.querySelectorAll('[data-tab]').forEach(b=>b.classList.toggle('active',b.dataset.tab===selected));
  const main=$('content');main.replaceChildren();
  if(selected==='alerts'){
    const choices=feed.events.filter(e=>e.kind==='choice'),completed=feed.events.filter(e=>e.kind==='complete');
    main.append(section('等待选择'));if(!choices.length)addEmpty(main,'暂时没有等待选择的会话。');choices.slice(0,20).forEach(e=>main.append(showEvent(e)));
    main.append(section('积分临期'));const soon=alerts.filter(a=>a.days<=1);if(!soon.length)addEmpty(main,'今天和明天没有已知的临期积分。');soon.slice(0,30).forEach(a=>main.append(showQuota(a)));
    main.append(section('最近完成'));if(!completed.length)addEmpty(main,'暂无最近完成的会话。');completed.slice(0,8).forEach(e=>main.append(showEvent(e)));
  } else if(selected==='sessions'){
    main.append(action('打开全部会话',()=>nav({view:'sessions'}),'action'),section('最近会话'));
    if(!feed.recent.length)addEmpty(main,'暂无可显示的会话。');
    feed.recent.forEach(s=>main.append(showRecent(s)));
  } else if(selected==='todo')renderTodos(main);
  badge();
}
async function announce(text){
  if(!settings.voice)return;
  try{await invoke('speak',{text})}catch(e){notice('语音播报不可用：'+String(e))}
}
async function incoming(e){
  const label=e.kind==='choice'?'等待选择':'本轮完成';
  notice(e.label+' '+label+' · '+(e.summary||e.title||''));
  if(kind==='panel'){selected='alerts';if(view==='tab')await expand()}
  announce(e.label+'，'+label+'。'+(e.summary||''));
  if(e.kind==='choice'&&e.can_focus&&settings.focus){try{await invoke('open',{sid:e.sid})}catch(err){notice('唤起应用失败：'+String(err))}}
}
async function refresh(force=false){
  if(pollBusy)return;pollBusy=true;
  try{
    const data=await invoke('feed');
    const rows=Array.isArray(data.events)?data.events.filter(e=>enabledEvents.has(e.kind)):[];
    const fresh=[];
    for(const e of rows){
      const id=e.sid+'|'+e.kind+'|'+e.at;
      if(!settings.seen[id]&&started&&e.at>=lastTrigger-2&&e.at>now()-180)fresh.push(e);
      if(leader())settings.seen[id]=e.at;
    }
    const cutoff=now()-86400;for(const [id,t] of Object.entries(settings.seen))if(t<cutoff)delete settings.seen[id];
    feed={events:rows,recent:Array.isArray(data.recent)?data.recent:[]};lastTrigger=now();started=true;
    if(fresh.length&&leader()){save();await incoming(fresh.sort((a,b)=>b.at-a.at)[0])}
    if(force)notice('会话已刷新');render();
  }catch(e){notice('会话服务暂时不可用：'+String(e));}
  finally{pollBusy=false}
}
async function refreshQuota(){
  if(quotaBusy)return;quotaBusy=true;
  try{
    const state=await window.__TAURI__.core.invoke('get_state');alerts=Array.isArray(state.alerts)?state.alerts:[];
    const ids=new Set(alerts.filter(a=>a.days<=1).map(a=>[a.app_id,a.name,a.expires,a.label].join('|')));
    if(previous!==null){const added=[...ids].filter(id=>!previous.has(id));if(added.length&&leader()){notice('有积分即将到期');announce('有积分即将到期，请查看桌面助手。');if(kind==='panel'&&view==='tab'){selected='alerts';await expand()}}}
    previous=ids;render();
  }catch(e){notice('积分数据暂时不可用：'+String(e))}
  finally{quotaBusy=false}
}
function tickTodos(){
  if(!leader())return;
  for(const t of settings.todos){if(!t.done&&!t.notified&&t.at<=Date.now()){t.notified=true;save();notice('待办提醒 · '+t.text);announce('待办提醒，'+t.text);selected='todo';if(kind==='panel'&&view==='tab')expand();render()}}
}
document.querySelectorAll('[data-tab]').forEach(b=>b.addEventListener('click',()=>{selected=b.dataset.tab;render()}));
let suppressDragClick=false;
$('handle').addEventListener('click',e=>{if(suppressDragClick){e.preventDefault();suppressDragClick=false;return}expand()});
$('edge-tab').addEventListener('click',expand);$('edge-tab').addEventListener('mouseenter',expand);
$('collapse').addEventListener('click',collapse);
$('close-compact').addEventListener('click',()=>{releaseLeader();invoke('close')});
$('close-panel').addEventListener('click',()=>{releaseLeader();invoke('close')});
$('open-settings').addEventListener('click',()=>nav({view:'settings'}));
$('pin').addEventListener('click',async()=>{
  const previous=settings.pin;settings.pin=!previous;clearTimeout(collapseTimer);
  try {
    await window.__TAURI__.event.emitTo('main','companion-pin-changed',{pin:settings.pin});
    save();render();
  } catch(e){settings.pin=previous;notice('无法保存固定状态：'+String(e));render()}
});
$('panel').addEventListener('mouseenter',()=>clearTimeout(collapseTimer));$('panel').addEventListener('mouseleave',scheduleCollapse);
for(const id of ['handle','drag-bar']){
  $(id).addEventListener('dragstart',e=>e.preventDefault());
  $(id).addEventListener('pointerdown',e=>{
    if(e.button!==0 || (id==='drag-bar'&&e.target.closest('button')))return;
    if(id==='handle')suppressDragClick=false;
    const x=e.screenX,y=e.screenY;
    const move=event=>{if(Math.hypot(event.screenX-x,event.screenY-y)<5)return;document.removeEventListener('pointermove',move);clearTimeout(collapseTimer);
      if(id==='handle')suppressDragClick=true;
      invoke('drag').then(async()=>{anchor=null;await layout(kind==='panel'?'panel':'ball')}).catch(err=>notice(String(err)));};
    const stop=()=>{document.removeEventListener('pointermove',move);document.removeEventListener('pointerup',stop);document.removeEventListener('pointercancel',stop)};
    document.addEventListener('pointermove',move);
    document.addEventListener('pointerup',stop);
    document.addEventListener('pointercancel',stop);
  });
}
window.addEventListener('keydown',e=>{if(e.key==='Escape'&&view==='panel')collapse()});
window.addEventListener('resize',()=>{if(view==='panel')render()});
(async()=>{
  theme();try{anchor=JSON.parse(localStorage.getItem(positionKey)||'null')}catch(_){anchor=null}
  if(!anchor&&kind==='orb')try{anchor=JSON.parse(localStorage.getItem('cockpit-companion-position')||'null')}catch(_){}
  await layout(kind==='panel'?'panel':'ball',!anchor);
  await window.__TAURI__.event.listen('companion-expand',()=>expand());
  await window.__TAURI__.event.listen('companion-settings-changed',event=>{
    const value=event.payload||{};
    for(const property of ['voice','focus','pin','theme','edge_hint'])if(value[property]!==undefined)settings[property]=value[property];
    save();theme();render();
  });
  try {
    const cfg=await window.__TAURI__.core.invoke('get_settings');
    const value=cfg.desktop_companion||{};
    for(const property of ['voice','focus','pin','theme','edge_hint'])if(value[property]!==undefined)settings[property]=value[property];
    save();theme();render();
  } catch (_) {}


  refresh();refreshQuota();tickTodos();
  setInterval(refresh,10000);setInterval(refreshQuota,60000);setInterval(tickTodos,1000);
})().catch(e=>notice(String(e)));
