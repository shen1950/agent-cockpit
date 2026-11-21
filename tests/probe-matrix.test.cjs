/* 无浏览器接线检查；真实函数与异步会话行为见 probe-matrix-behavior.test.cjs。 */
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const root = path.resolve(__dirname, '..');
const read = (p) => fs.readFileSync(path.join(root, p), 'utf8');
const html = read('ui/index.html');
const js = read('ui/app.js');
const css = read('ui/styles.css');
const commands = read('src-tauri/src/commands.rs');
const keys = read('src-tauri/src/keys.rs');

// 骨架必须唯一，取消按钮必须是可操作控件，不能只补一段说明。
const ids = [...html.matchAll(/\bid="([^"]+)"/g)].map((m) => m[1]);
for (const id of ['mxmodal', 'mx-title', 'mx-sub', 'mx-head', 'mx-body', 'mx-close', 'mx-cancel', 'mx-all']) {
  assert.equal(ids.filter((v) => v === id).length, 1, `骨架 #${id} 缺失或重复`);
}
assert.match(html, /<button\b[^>]*id="mx-cancel"[^>]*>取消检测<\/button>/);
assert.match(html, /已发出的请求[\s\S]*15 秒超时[\s\S]*并非立即中断 HTTP/);
assert.match(js, /openMatrix\(p\.id,\s*\{\s*type:\s*'endpoint',\s*value:\s*b\.id\s*\}\)/);
assert.doesNotMatch(js, /type:\s*'fmt'/, '接入点入口不能继续按协议聚合');
assert.match(js, /function keyChat\(siteId, id\)\s*\{\s*return openMatrix\(siteId,\s*\{\s*type:\s*'key',\s*value:\s*id\s*\}\);\s*\}/);
assert.doesNotMatch(js, /action:\s*'key_chat'/, '旧请求入口不能继续盲选协议和首模型');

// 前端只发送组合选择和记录ID；后台必须接收检测与取消两个 action。
assert.match(js, /const payload = \{ action: 'key_matrix', site: site\.id, request_id: request\.id, choices \}/);
assert.match(js, /if \(filter\.keys\) payload\.keys = filter\.keys/);
assert.match(js, /if \(filter\.endpoints\) payload\.endpoints = filter\.endpoints/);
assert.doesNotMatch(js, /payload\.fmts\s*=/);
assert.match(js, /action: 'key_matrix_cancel', request_id: request\.id/);
assert.match(commands, /"key_matrix"\s*=>/);
assert.match(commands, /"key_matrix_cancel"\s*=>/);
assert.ok(commands.includes('keys::probe_matrix_cancellable('), '矩阵必须走可取消的调度器');
assert.match(keys, /pub fn probe_matrix\(/);
for (const field of ['endpoint_id', 'model_catalogs', 'request_id', 'choices']) {
  assert.match(keys + commands, new RegExp(`\\b${field}\\b`), `后端合同字段缺失：${field}`);
}

for (const state of ['ok', 'key_denied', 'site_disabled', 'model_mismatch', 'unt', 'unconfigured', 'rate_limited', 'cancelled', 'invalid_response']) {
  assert.match(js, new RegExp(`\\b${state}:`), `前端状态标签缺失：${state}`);
  assert.match(css, new RegExp(`\\.mxst\\.${state}\\b`), `状态样式缺失：${state}`);
  if (state !== 'unt') assert.match(html, new RegExp(`class="mxst ${state}"`), `图例缺失：${state}`);
}
assert.match(js, /\$\('mx-close'\)\.addEventListener\('click', closeMatrix\)/);
assert.match(js, /\$\('mx-cancel'\)\.addEventListener\('click', \(\) => mxCancel\(\)\)/);
assert.match(js, /if \(!\$\('mxmodal'\)\.hidden\) \{ e\.preventDefault\(\); e\.stopPropagation\(\); closeMatrix\(\); return; \}/);
const acquisitionActions = [...js.matchAll(/action:\s*'site_models_all'/g)];
assert.equal(acquisitionActions.length, 1, '模型获取只能保留站点统一 action 出口');
assert.match(js, /汇总不代表每把密钥权限/);
console.log('probe-matrix: 接线检查通过');
