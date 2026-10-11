/* 离线验证粘贴解析：node paste-test.js */
const fs = require('fs');
const path = require('path');
const src = fs.readFileSync(path.join(__dirname, '..', 'ui', 'app.js'), 'utf8');
// 只取解析相关函数的源码，剥掉 DOM 依赖。
// 终点是解析区后面的第一个函数（quotaFromText），别再锚某个可能被重构删掉的函数名
const start = src.indexOf('const PASTE_URL_RE');
const end = src.indexOf('function quotaFromText');
let code = src.slice(start, end);
code += '\nmodule.exports = { parseApiText, stripMd };\n';
const mod = { exports: {} };
new Function('module', 'exports', code)(mod, mod.exports);
const { parseApiText } = mod.exports;

const messy = `
Longcat

LongCat-2.0
LongCat-2.5-Preview

● OpenAI格式：
https://api.longcat.chat/openai/v1/chat/completions
● Anthropic格式：https://api.longcat.chat/anthropic
OpenAI API 格式
● ● 对话补全：/v1/chat/completions
Anthropic API 格式
● ● 兼容Anthropic Claude API规范，支持以下接口：
● ● 消息对话：/v1/messages

TieZ
sk-tiez-0123456789abcdefghij

Pot
注明这是一个 Pot 用的 key
sk-pot-abcdefghijklmnopqrstuvwx

WorkBuddy
沉浸式翻译
sk-wb-aaaabbbbccccddddeeee

OpenCode
sk-oc-1111222233334444

QoderCN
sk-qcn-5555666677778888

Aliwelink API
image-2
sk-ali-9999000011112222
Pro
sk-pro-3333444455556666
`;

const r = parseApiText(messy);
console.log('站点名:', JSON.stringify(r.site.name));
console.log('类型:', r.site.kind);
console.log('第一个 url:', r.site.base_url);
console.log('全局模型:', r.site.models_list);
console.log('接入点清单:', JSON.stringify(r.site.bases));
console.log('识别到 key 数:', r.keys.length);
for (const k of r.keys) {
  console.log(`  - 用途=${(k.label || '(无)').padEnd(10)} key=${k.key.padEnd(30)} base=${k.base_url || '(无)'}`);
}
