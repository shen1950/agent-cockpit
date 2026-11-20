/* 「获取模型」只有一个入口的接线测试（2026-11-20 用户要求）：
   模型清单是站点级的，不分协议存。协议行里各摆一个同名按钮既重复，
   「点哪一个」本身也说不清。这里守住三件事：

   ① 协议行的循环体里不能再出现「获取模型」；
   ② 站点卡里只出现一次（三元分支的两个写法只能有一个渲染出来，按字面各算一次）；
   ③ 前端发的 action 与后端分支、探测函数同名对得上。
*/

const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const root = path.resolve(__dirname, '..');
const read = (p) => fs.readFileSync(path.join(root, p), 'utf8');

const js = read('ui/app.js');
const commands = read('src-tauri/src/commands.rs');
const keys = read('src-tauri/src/keys.rs');

/* ① 协议行循环体里不许有 */
const start = js.indexOf('for (const b of bases)');
assert.ok(start > 0, '找不到协议行循环 for (const b of bases)');
const chipsAt = js.indexOf('const chips = el(', start);
assert.ok(chipsAt > start, '找不到协议行循环的结尾锚点（const chips）');
const rowBlock = js.slice(start, chipsAt);
// 只看真正的按钮调用，注释里提到「获取模型」不算
assert.ok(
  !rowBlock.includes("btn('获取模型'"),
  '协议行里还留着「获取模型」：一个站点就会冒出多个同名按钮'
);
assert.ok(
  !rowBlock.includes('pullSiteModels('),
  '协议行还在调旧的按协议拉取 pullSiteModels'
);

/* ② 站点卡里只该有一个入口（三元的两支写法算同一处，断言允许 2 次字面出现） */
const siteInfoAt = js.indexOf('function siteInfo(p)');
assert.ok(siteInfoAt > 0, '找不到 siteInfo');
const siteInfoBlock = js.slice(siteInfoAt, js.indexOf('async function pullSiteModelsAll', siteInfoAt));
const hits = (siteInfoBlock.match(/btn\('获取模型'/g) || []).length;
assert.ok(hits >= 1 && hits <= 2, `siteInfo 里「获取模型」出现 ${hits} 次，应为一个入口（三元两支）`);

/* ③ 前后端对得上 */
assert.ok(js.includes('function pullSiteModelsAll('), 'pullSiteModelsAll 未定义');
assert.ok(js.includes("action: 'site_models_all'"), '前端没有发 site_models_all');
assert.ok(commands.includes('"site_models_all" =>'), '后端没有 site_models_all 分支');
assert.ok(commands.includes('keys::probe_models_all('), '后端没走全站拉取');
assert.ok(keys.includes('pub fn probe_models_all('), 'probe_models_all 未实现');

/* ④ 多协议、多密钥都得覆盖到：一个口失败不能把整次拉取判死 */
assert.ok(
  keys.includes('for (url, fmt) in &bases'),
  'probe_models_all 应按接入点逐个问，漏了协议多的站点'
);
assert.ok(
  keys.includes('break;') && keys.includes('attempts'),
  'probe_models_all 应按密钥顺序试到一把能用，漏了「某把密钥只对某个口开放」'
);

console.log('site-models-single-entry: 全部接线检查通过');
