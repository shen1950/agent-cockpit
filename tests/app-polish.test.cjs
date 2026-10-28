const {test}=require('node:test');
const assert=require('node:assert/strict');
const fs=require('node:fs');
const path=require('node:path');
const vm=require('node:vm');
const source=fs.readFileSync(path.join(__dirname,'../ui/app.js'),'utf8');
function fixture(view='settings'){
  const context={S:{view},DESC_INLINE_MAX:120,el:(tag,attrs,...children)=>({tag,...attrs,children:children.flat().filter(x=>x!=null&&x!==false)}),qhelp:text=>({tag:'help',text})};
  vm.createContext(context);
  for(const name of ['row','rowWide']){
    const body=source.match(new RegExp(`function ${name}\\(label, desc, ctl\\) \\{[\\s\\S]*?\\n\\}`));
    assert.ok(body);vm.runInContext(body[0],context);
  }
  return context;
}
const nodes=n=>typeof n!=='object'?[]:[n,...(n.children||[]).flatMap(nodes)];
test('short and long setting descriptions are available on demand without gray paragraphs',()=>{
  const c=fixture();
  for(const fn of ['row','rowWide'])for(const text of ['开启后自动刷新。','读取当前账号的真实额度。'.repeat(20)]){
    const row=c[fn]('自动刷新',text,{tag:'button'});const all=nodes(row);
    assert.equal(all.some(x=>x.class==='desc'),false);assert.equal(all.find(x=>x.tag==='help').text,text);
    assert.ok(all.some(x=>x.tag==='button'));
  }
});
test('remote control retains its existing inline description layout',()=>{
  const c=fixture('remote');const row=c.row('状态','当前监听地址',{tag:'value'});
  assert.ok(nodes(row).some(x=>x.class==='desc'));assert.equal(nodes(row).some(x=>x.tag==='help'),false);
});
