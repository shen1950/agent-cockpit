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
const d = { apps: [{id:'traework-cn', name:'TRAE CN'}], proxy: { running: false, pools: [{ id:'p', name:'多来源组', members:[{src:'a'},{src:'b'}] }], dist:[{id:'key', name:'测试密钥'}] } };
let modal = null, copied = '';
const context = {
  S: {data:d, proxyPage:{ models:{groups:[{owner:'trae',models:[{id:'trae/glm-5.3-flash'}]}]} }},
  document:{getElementById: () => null}, $: () => host, clear: n => {n.children=[];}, el:make,
  btn: (label, cls, fn) => new Node('button', {class:cls, onclick:fn}, label),
  pxNote: s => make('p', {}, s), chip:s => make('span', {}, s), pxRunBtn:() => make('button', {}, '启动反代'),
  pxRow:(label, value, actions) => make('div', {}, label, value, actions), proxyBase:()=> 'http://127.0.0.1:8788/v1',
  guideAgents:()=>[{app:'traework-cn', detail:'1 个可用'}, {app:'opencode', usable:1, detail:'3 个模型'}], PX_OWNER_NAME:{trae:'TRAE CN'},
  openPoolModal:g=>{modal=g || 'new';}, openDistModal:()=>{}, loadProxyModels:()=>{}, render:()=>{}, toast:()=>{},
  copyText:s=>{copied=s;}, invoke:async()=>({ok:true,msg:'test-key'}), renderGuide:()=>{},
};
vm.createContext(context);
vm.runInContext(fs.readFileSync('ui/proxy-guide.js','utf8'), context);
context.renderGuide(d);
assert.match(text(host), /多个来源/);
let details = nodes(host).find(n => n.tag === 'details');
details.open = true; details.events.toggle();
context.renderGuide(d); // periodic state refresh
details = nodes(host).find(n => n.tag === 'details');
assert.equal(details.open, true, 'source list stays open after refresh');
const bridge = nodes(host).find(n => n.class === 'guide-agent' && text(n).includes('OpenCode'));
assert.match(text(bridge), /桥可用/);
assert.doesNotMatch(text(bridge), /未检测到/);
button('编辑成员').click(); assert.equal(modal.id, 'p');
button('下一步：选择模型').click();
assert.match(text(host), /trae\/glm-5.3-flash/);
const input = nodes(host).find(n => n.tag === 'input');
input.value = 'trae/glm-5.3-flash'; input.events.input();
button('下一步：启动与密钥').click();
assert.match(text(host), /不会接管或修改任何客户端/);
button('上一步').click(); assert.match(text(host), /模型目录/);
button('下一步：启动与密钥').click();
button('下一步：配置客户端').click();
assert.match(text(host), /不要求填回 TRAE/);
assert.match(text(host), /trae\/glm-5.3-flash/);
button('复制').click(); assert.equal(copied, 'http://127.0.0.1:8788/v1');
button('上一步').click(); assert.match(text(host), /接入密钥/);
button('上一步').click(); button('上一步').click();
assert.equal(nodes(host).find(n => n.tag === 'details').open, true, 'source list stays open after returning');
console.log('proxy guide interaction checks passed');
