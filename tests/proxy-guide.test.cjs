const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

class Node {
  constructor(tag, attrs = {}, ...children) { Object.assign(this, attrs); this.tag = tag; this.children = children.flat().filter(x => x != null); this.events = {}; }
  appendChild(child) { this.children.push(child); }
  addEventListener(name, fn) { this.events[name] = fn; }
  click() { (this.events.click || this.onclick)?.(); }
}
const host = new Node('host');
const make = (tag, attrs, ...kids) => new Node(tag, attrs, ...kids);
const nodes = n => n instanceof Node ? [n, ...n.children.flatMap(nodes)] : [];
const text = n => n instanceof Node ? n.children.map(text).join(' ') : String(n ?? '');
const button = label => nodes(host).find(n => n.tag === 'button' && text(n) === label);
const d = { apps: [{id:'traework-cn', name:'TRAE CN'}], proxy: { running: false, sources: [{id:'a',name:'来源 A'},{id:'b',name:'来源 B'}], dist:[{id:'key', name:'测试密钥'}] } };
let modal = null, copied = '';
let mounted = null;
const context = {
  S: {data:d, proxyPage:{ models:{groups:[{owner:'trae',models:[{id:'trae/glm-5.3-flash'}]}]} }},
  window: { supplierPage: { providers: [{ id: 'a', name: '来源 A' }, { id: 'b', name: '来源 B' }] } },
  document:{getElementById: () => null}, $: () => host, clear: n => {n.children=[];}, el:make,
  btn: (label, cls, fn) => new Node('button', {class:cls, onclick:fn}, label),
  pxNote: s => make('p', {}, s), chip:s => make('span', {}, s), pxRunBtn:() => make('button', {}, '启动反代'),
  pxRow:(label, value, actions) => make('div', {}, label, value, actions), proxyBase:()=> 'http://127.0.0.1:8788/v1',
  guideAgents:()=>[{app:'traework-cn', detail:'1 个可用'}, {app:'opencode', usable:1, detail:'3 个模型'}], PX_OWNER_NAME:{trae:'TRAE CN'},
  openDistModal:()=>{}, loadProxyModels:()=>{}, render:()=>{}, toast:()=>{},
  copyText:s=>{copied=s;}, invoke:async()=>({ok:true,msg:'test-key'}), renderGuide:()=>{},
  jump:()=>{}, gatewayClient:{ mount:(hostEl, dd, opts)=>{ mounted = opts; } },
  // ui/legal.js 提供：反代卡片头部的常驻风险条
  proxyRiskNote:()=>make('div', {class:'legalnote'}, make('b', {}, '用前须知')),
};
vm.createContext(context);
vm.runInContext(fs.readFileSync('ui/proxy-guide.js','utf8'), context);
context.renderGuide(d);
assert.match(text(host), /来源 A/);
assert.match(text(host), /来源 B/);
const bridge = nodes(host).find(n => n.class === 'guide-agent' && text(n).includes('OpenCode'));
assert.match(text(bridge), /桥可用/);
assert.doesNotMatch(text(bridge), /未检测到/);
button('下一步：选择模型').click();
assert.match(text(host), /trae\/glm-5.3-flash/);
const input = nodes(host).find(n => n.tag === 'input');
input.value = 'trae/glm-5.3-flash'; input.events.input();
button('下一步：启动与密钥').click();
assert.match(text(host), /不会接管或修改任何客户端/);
button('上一步').click(); assert.match(text(host), /模型目录/);
button('下一步：启动与密钥').click();
button('下一步：接入本地网关').click();
assert.match(text(host), /接入 Agent/);
assert.equal(mounted.app, 'traework-cn', '最后一步要把所选 Agent 交给共用的接入本地网关设置');
assert.equal(mounted.keyId, 'key', '唯一启用的接入密钥要自动带上');
assert.equal(mounted.model, 'trae/glm-5.3-flash', '所选模型要交给共用的接入本地网关设置');
button('复制地址').click(); assert.equal(copied, 'http://127.0.0.1:8788/v1');
button('上一步').click(); assert.match(text(host), /接入密钥/);
console.log('proxy guide interaction checks passed');
