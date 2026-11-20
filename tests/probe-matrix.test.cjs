/* 接入点体检（方案 A）的接线测试：不启浏览器，只验「该连的地方都连上了」。
   为什么不做 Playwright 渲染测试：本机对反复创建 Chromium 配置有硬限制
   （见 AGENTS.md 的账户锁定条款），一个弹窗的接线不值得冒这个风险。
   渲染与交互靠人工过一遍，这里守住三个容易在重构里断掉的接口：

   ① 弹窗骨架在 index.html 里（缺了就是点了没反应）；
   ② 两个入口都调 openMatrix，且聚焦的维度不同（接入点→协议列、密钥→密钥行）；
   ③ 前端发的 action 名与后端 match 的分支、以及批量探测函数同名对得上。
*/

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

/* ① 弹窗骨架 */
for (const id of ['mxmodal', 'mx-title', 'mx-sub', 'mx-head', 'mx-body', 'mx-close', 'mx-all']) {
  assert.ok(html.includes(`id="${id}"`), `index.html 缺少 #${id}`);
}
assert.ok(html.includes('接入点体检'), '弹窗标题文案缺失');

/* ② 两个入口分别聚焦不同维度 */
assert.ok(
  /openMatrix\([^)]*type:\s*'fmt'/.test(js),
  '接入点行应调用 openMatrix 并聚焦到协议列'
);
assert.ok(
  /openMatrix\([^)]*type:\s*'key'/.test(js),
  '密钥行应调用 openMatrix 并聚焦到密钥行'
);
assert.ok(js.includes('function openMatrix('), 'openMatrix 未定义');
assert.ok(js.includes('function renderMatrix('), 'renderMatrix 未定义');
assert.ok(js.includes('function mxRun('), 'mxRun 未定义');

/* ③ 前后端 action 名与探测函数对得上 */
assert.ok(js.includes("action: 'key_matrix'"), '前端没有发 key_matrix');
assert.ok(commands.includes('"key_matrix" =>'), '后端没有 key_matrix 分支');
assert.ok(commands.includes('keys::probe_matrix('), '后端没走批量探测');
assert.ok(keys.includes('pub fn probe_matrix('), 'probe_matrix 未实现');
assert.ok(keys.includes('pub fn fmt_of_url('), 'fmt_of_url 未实现');
assert.ok(keys.includes('pub fn matrix_columns('), 'matrix_columns 未实现');

/* ④ 状态语义：这几种必须能区分开，否则矩阵会把协议问题说成密钥问题 */
for (const state of ['ok', 'key_denied', 'site_disabled', 'model_mismatch', 'unt']) {
  assert.ok(js.includes(`${state}:`), `前端缺少状态 ${state} 的文案`);
  assert.ok(css.includes(`.mxst.${state}`), `样式缺少 .mxst.${state}`);
}
const legendStates = ['ok', 'key_denied', 'site_disabled', 'model_mismatch'];
for (const s of legendStates) {
  assert.ok(
    new RegExp(`class="mxst ${s}"`).test(html),
    `图例缺少 ${s}，用户没法判断「站点停用」不等于「密钥坏了」`
  );
}

/* ⑤ 等待态不能被旧结论顶替：失败要退回未检测 */
assert.ok(js.includes('resetPending'), '检测失败时缺少回退逻辑');

console.log('probe-matrix: 全部接线检查通过');
