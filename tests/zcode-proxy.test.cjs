const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const app = fs.readFileSync(path.join(__dirname,'../ui/app.js'),'utf8');
const zcode = fs.readFileSync(path.join(__dirname,'../ui/zcode.js'),'utf8');
function extract(name) {
  const start=app.indexOf(`function ${name}(`); assert.ok(start>=0);
  const next=app.indexOf('\nfunction ',start+1);
  return app.slice(start,next<0?undefined:next);
}
test('ZCode is discoverable in the proxy wizard with its own model namespace',()=>{
  const c={S:{},Map};vm.createContext(c);
  vm.runInContext(extract('proxyKind')+'\n'+extract('guideModels')+'\n'+extract('guideAccounts')+'\n'+extract('guideAgents'),c);
  const d={apps:[{id:'zcode',name:'ZCode',installed:true}],proxy:{zcode:{installed:true,accounts:[
    {name:'a',usable:true,models:[{id:'GLM-5.3-Flash'}]}, {name:'b',usable:false,models:[]}
  ]}}};
  assert.equal(c.proxyKind('zcode'),'zc'); assert.equal(c.guideAgents(d)[0].usable,1);
  assert.equal(c.guideModels(d,'zcode').models[0].id,'GLM-5.3-Flash');
  assert.equal(c.guideAccounts(d,{kind:'zc'}).length,2);
});
test('ZCode 的逐模型接入面板已随「模型反代」收进总页，向导那套映射还在',()=>{
  // 2026-11-03：每个 Agent 页的「模型反代」整块砍掉，ZCode 的 Start Plan 面板一并退场；
  // 挑模型/测试现在只在「模型反代 → 反代全景」里做，所以这两个入口函数不该再出现。
  assert.equal(zcode.includes('renderZcodeProxyPanel'), false);
  assert.equal(zcode.includes('zcodeProxyAction'), false);
  assert.match(zcode, /function zcodeProvider\(/, '向导与服务商标签还要用它');
});
