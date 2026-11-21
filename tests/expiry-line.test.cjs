const {test}=require('node:test');
const assert=require('node:assert/strict');
const fs=require('node:fs');
const path=require('node:path');
const vm=require('node:vm');
const source=fs.readFileSync(path.join(__dirname,'../ui/app.js'),'utf8');

/* 逐笔明细那两列（到期时间 / 耗尽时间）全站只有一套写法。
   2026-11-21 之前 WorkBuddy 的历史笔把厂商 ExpiredTime 当到期日，233 笔里 204 笔
   显示成"提前作废"；现在到期日只认周期结束，ExpiredTime 单列成耗尽时间，没耗尽写 ——。
   这个文件钉住文案：三处明细渲染都必须走 expiryLine，别哪个页面又自己拼一套。 */
function fixture(){
  const context={el:(tag,attrs,...children)=>({tag,...attrs,children:children.flat().filter(x=>x!=null&&x!==false)})};
  vm.createContext(context);
  const pick=(re,name)=>{const m=source.match(re);assert.ok(m,`没找到 ${name}`);vm.runInContext(m[0],context);};
  pick(/const EXP_NONE = [^;]+;/,'EXP_NONE');
  for(const name of ['dtxt','expiryLine'])pick(new RegExp(`function ${name}\\([^)]*\\) \\{[\\s\\S]*?\\n\\}`),name);
  return context;
}
const texts=(n)=>typeof n==='string'?[n]
  :(n&&typeof n==='object'?[...(n.text?[String(n.text)]:[]),...(n.children||[]).flatMap(texts)]:[]);

test('到期与耗尽两个时刻分开写，顺序与写法固定',()=>{
  const c=fixture();
  const line=c.expiryLine({expires_on:'2026-11-21 06:03:06',ended_on:'2026-11-20 21:20:04'});
  const t=texts(line).join('');
  assert.ok(t.includes('到期时间 2026-11-21 06:03'),t);
  assert.ok(t.includes('耗尽时间 2026-11-20 21:20'),t);
  assert.ok(t.indexOf('到期时间')<t.indexOf('耗尽时间'),'到期在前、耗在后');
});

test('没耗尽的只写到到期时间，不摆空的耗尽列',()=>{
  const c=fixture();
  const t1=texts(c.expiryLine({expires_on:'2026-11-22 15:48',ended_on:''})).join('');
  assert.ok(t1.includes('到期时间 2026-11-22 15:48'),t1);
  assert.equal(t1.includes('耗尽时间'),false,`没有耗尽时间时不该出现这一列：${t1}`);
  const t2=texts(c.expiryLine({expires_on:'',ended_on:''})).join('');
  assert.ok(t2.includes('到期时间 未给出'),t2);
  assert.equal(t2.includes('耗尽时间'),false,t2);
});

test('所有明细渲染都走同一套写法，不再各自拼到期时间',()=>{
  const calls=(source.match(/expiryLine\(/g)||[]).length;
  assert.ok(calls>=3,`expiryLine 至少被三处明细调用，实际 ${calls}`);
  const legacy=source.match(/`到期时间 \$\{[^}]*expires_on[^}]*\}`/g)||[];
  assert.deepEqual(legacy,[],`还有地方直接拼到期时间：${legacy.join(' / ')}`);
});
