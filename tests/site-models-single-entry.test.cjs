/* 获取模型入口与落账合同。真实请求/组合行为另由 Rust HTTP 和前端 VM 测试覆盖。 */
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const root = path.resolve(__dirname, '..');
const read = p => fs.readFileSync(path.join(root, p), 'utf8');
const js = read('ui/app.js');
const commands = read('src-tauri/src/commands.rs');
const keys = read('src-tauri/src/keys.rs');

const siteStart = js.indexOf('function siteInfo(p)');
const pullStart = js.indexOf('async function pullSiteModelsAll', siteStart);
assert.ok(siteStart > 0 && pullStart > siteStart, '站点信息与模型获取入口必须存在');
const block = js.slice(siteStart, pullStart);
// 必须从 siteInfo 内定位循环，不能误命中前面矩阵中的同名循环。
const loopStart = block.indexOf('for (const b of bases)');
const chipsStart = block.indexOf('const chips = el(', loopStart);
assert.ok(loopStart >= 0 && chipsStart > loopStart, '必须找到站点卡片里的协议行边界');
assert.ok(!block.slice(loopStart, chipsStart).includes("btn('获取模型'"), '协议行不应重复放获取模型');
assert.ok(!block.slice(loopStart, chipsStart).includes('pullSiteModels('), '协议行不应调用旧的模型获取入口');
const buttonLiterals = (block.match(/\bbtn\([^;\n]*'获取模型'/g) || []).length;
assert.ok(buttonLiterals >= 1 && buttonLiterals <= 2, '站点模型区保留一个入口（兼容互斥分支）');
assert.ok(js.includes("action: 'site_models_all'"), '必须调用站点单入口');
assert.ok(commands.includes('keys::discover_site_models('), '必须逐Key发现，不能只试到第一把成功为止');
assert.ok(commands.includes('merge_model_discoveries('), '必须按最新配置合并每Key目录');
assert.ok(commands.includes('"model_catalogs"'), '不能只存站点模型并集');
assert.ok(keys.includes('pub fn models_for_endpoint('), '检测必须支持Key和端点范围的模型目录');
assert.ok(keys.includes('pub fn matrix_endpoints('), '端点不能仅以协议名去重');
const actionsStart = commands.indexOf('if act.starts_with("chatgpt_")');
const guardStart = commands.indexOf('let _g = cmd_guard();', actionsStart);
const lockFreeRoutes = commands.slice(actionsStart, guardStart);
for (const action of ['key_matrix', 'key_matrix_cancel', 'site_models_all']) {
  assert.ok(lockFreeRoutes.includes(`"${action}"`), `${action} 不应在整个网络期间持全局锁`);
}
console.log('site-models-single-entry: 入口、按Key落账及锁范围合同通过');
