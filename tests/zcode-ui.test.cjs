const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
class Node {
  constructor(tag, attrs = {}, children = []) { this.tag = tag; Object.assign(this, attrs); this.children = children.flat().filter(x => x != null && x !== false); this.events = {}; this.value ??= ''; }
  appendChild(node) { this.children.push(node); return node; }
  addEventListener(name, fn) { this.events[name] = fn; }
  setAttribute(name, value) { this[name] = value; }
  remove() { this.removed = true; }
  focus() {}
}
const nodes = n => typeof n !== 'object' ? [] : [n, ...(n.children || []).flatMap(nodes)];
const text = n => typeof n !== 'object' ? String(n ?? '') : (n.children || []).map(text).join('');
const button = (n, title) => nodes(n).find(x => x.tag === 'button' && text(x) === title);
function setup(options = {}) {
  const requests = [], notices = [], confirms = [], dom = new Map();
  for (const id of ['im-save','im-nav','im-body']) dom.set(id, new Node('div'));
  const context = {
    Set, console, S: { data: { instances: [] }, cur: 'zcode', detail: { sec: 'auth' } },
    el: (tag, attrs, ...children) => new Node(tag, attrs, children),
    btn: (label, cls, fn) => new Node('button', { class: cls, action: fn }, [label]),
    qhelp: hint => new Node('button', { class: 'qhelp', hint }, ['?']),
    chatgptInfoRow: (label, value, hint) => new Node('row', { hint }, [label,value]),
    secPanel: (title, ...children) => new Node('section', {}, [title,...children]),
    quotaPanel: () => new Node('quota'), quotaBox: () => new Node('quota'), stampTime: () => '2026-10-27 20:24', refreshQuota: async () => {},
    document: { body: new Node('body') }, navigator: { clipboard: { writeText: async () => {} } },
    $: id => dom.get(id), clear: node => { node.children = []; }, restoreScroll: () => {},
    toast: (...args) => notices.push(args), load: async () => {}, render: () => {}, renderDetail: () => {},
    openDetail: async (...args) => requests.push({ openDetail: args }), renameAccount: () => {},
    askConfirm: async (message, settings) => { confirms.push({message,settings}); return options.confirm !== false; },
    closeDetailNow: () => {}, del: async () => true,
    invoke: async (method, args) => {
      requests.push(args.payload);
      return options.respond ? options.respond(args.payload) : { ok: true, msg: '完成' };
    },
  };
  vm.createContext(context); vm.runInContext(fs.readFileSync(path.join(__dirname,'../ui/zcode.js'),'utf8'),context);
  const instance = { app:'zcode',name:'shen1950',data_dir:'C:/Users/Example/.zcode',quota:{},
    zcode_auth:{ provider:'bigmodel', account:'shen1950', has_auth:true, current:false, auth_status:'ready' } };
  return { context, requests, notices, confirms, dom, instance };
}
test('ZCode card opens its selected login instead of rebinding the active account', async () => {
  const s = setup(); const b=s.context.zcodeOpenButton(s.instance);
  assert.equal(text(b),'切换并打开'); await b.action();
  assert.deepEqual(JSON.parse(JSON.stringify(s.requests[0])),{action:'zcode_switch',app:'zcode',name:'shen1950'});
  const current=s.context.zcodeOpenButton({...s.instance,zcode_auth:{...s.instance.zcode_auth,current:true}});
  assert.equal(text(current),'打开窗口'); await current.action(); assert.equal(s.requests.at(-1).action,'launch_app');
});
test('switch requires explicit restart confirmation before a forced request', async () => {
  const s=setup({respond:p=>p.force?{ok:true}:{ok:false,need_force:true}});
  await s.context.zcodeAction('zcode_switch','shen1950');
  assert.equal(s.requests.length,2); assert.equal(s.requests[0].force,undefined); assert.equal(s.requests[1].force,true);
  assert.match(s.confirms[0].message,/保存/); assert.equal(s.confirms[0].settings.okText,'重启并切换');
});
test('cancelling a switch never sends a forced request', async () => {
  const s=setup({confirm:false,respond:()=>({ok:false,need_force:true})});
  const r=await s.context.zcodeAction('zcode_switch','shen1950');
  assert.equal(r.cancelled,true); assert.equal(s.requests.length,1);
});
test('ending a ZCode window confirms End without promising an account switch', async () => {
  const s=setup({respond:p=>p.force?{ok:true}:{ok:false,need_force:true}});
  const panel=s.context.zcodeDetailPanel({...s.instance,running:true,zcode_auth:{...s.instance.zcode_auth,current:true}},'run');
  await button(panel,'结束 ZCode').action();
  assert.equal(s.requests[0].action,'kill'); assert.equal(s.requests[1].force,true);
  assert.equal(s.confirms[0].settings.okText,'结束'); assert.doesNotMatch(s.confirms[0].message,/切换|重启/);
});
test('login view distinguishes platforms and does not offer logout for another account', () => {
  const s=setup(); const first=s.context.zcodeDetailPanel(s.instance,'auth');
  const second=s.context.zcodeDetailPanel({...s.instance,name:'Irshad Taj',zcode_auth:{provider:'zai',account:'Irshad Taj',has_auth:true,current:true}},'auth');
  assert.match(text(first),/BigModel/); assert.match(text(second),/Z.ai/);
  assert.equal(button(first,'退出登录'),undefined); assert.ok(button(second,'退出登录'));
});
test('missing login has capture actions rather than an unusable switch button', () => {
  const s=setup(); const panel=s.context.zcodeDetailPanel({...s.instance,zcode_auth:{has_auth:false,needs_login:true}},'auth');
  assert.equal(button(panel,'切换并打开'),undefined); assert.ok(button(panel,'保存当前登录')); assert.ok(button(panel,'打开客户端登录'));
});
test('removal is a registration operation with a visible data-preservation statement', async () => {
  const s=setup(); let removed;
  s.context.del=async (...args)=>{removed=args;return true;};
  const panel=s.context.zcodeDetailPanel(s.instance,'danger');
  assert.match(text(panel),/本机对话和项目保留/); await button(panel,'移除账号').action();
  assert.equal(removed[0],'zcode'); assert.equal(removed[1],'shen1950');
});
test('saved account import carries explicit identity and never switches the running client', async () => {
  const s=setup({respond:p=>p.action==='zcode_accounts'?{ok:true,accounts:[
    {user_id:'zcode:bigmodel:1',provider:'bigmodel',account:'shen1950',has_auth:true},
    {user_id:'zcode:zai:1',provider:'zai',account:'Irshad Taj',has_auth:true,registered_as:'Irshad Taj'},
  ]}:{ok:true,name:p.name}});
  const dialog=s.context.openZcodeAdd(); await new Promise(resolve=>setImmediate(resolve));
  assert.match(text(dialog),/BigModel/); assert.match(text(dialog),/Z.ai/);
  await button(dialog,'添加账号').action();
  const capture=s.requests.find(p=>p.action==='zcode_capture');
  assert.equal(capture.user_id,'zcode:bigmodel:1'); assert.equal(capture.name,'shen1950');
  assert.equal(s.requests.some(p=>['zcode_switch','zcode_logout','kill'].includes(p.action)),false);
});
test('an authoritative empty ZCode plan is shown as no plan while fresh errors remain visible', () => {
  const s=setup();
  const account={...s.instance,zcode_auth:{...s.instance.zcode_auth,quota_state:'no_plan'},quota:{fetched_at:1,source:'server',stale:false,live_error:''}};
  const panel=s.context.zcodeQuotaPanel(account,{compact:true});
  assert.match(text(panel),/暂无 ZCode 套餐/);assert.doesNotMatch(text(panel),/读不到|没读到|剩余 0/);
  const failed=s.context.zcodeQuotaPanel({...account,quota:{...account.quota,live_error:'HTTP 401',stale:true}});
  assert.equal(failed.tag,'quota');assert.doesNotMatch(text(failed),/暂无 ZCode 套餐/);
});
