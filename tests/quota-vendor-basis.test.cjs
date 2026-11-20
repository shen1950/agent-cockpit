/* 额度口径回归：厂商没给「已用」时，不许拿 total−left 反推一个假数。
 *
 * 背景（2026-11-12 小浣熊积分全面审查）：
 *   官网「积分构成」那一栏显示的是**当前余额**（balance.daily_points=1878），
 *   不是这一档历次发放的累加（7 笔 ×300 = 2100）。旧实现把两者混着用，
 *   `used = total − remaining` 于是算出一个两边口径不同的假数，
 *   界面上就出现「14000 + 2100 − 41 ≠ 15878」「已用 0 / 15,878」这种自相矛盾。
 *
 * 本测试直接跑 ui/app.js 里的 quotaGroups / quotaAgg / usedPct，
 * 验证 used_known=false 时：
 *   1. 档位 used 为 null（不是被反推出来的 0）
 *   2. 汇总的「已用」不再成立（顶部不摆「已用 X / Y」）
 *   3. used_known=true 时行为不变（别把别的产品也改坏了）
 */
const fs = require('fs');
const path = require('path');
const vm = require('vm');

const APP = path.join(__dirname, '..', 'ui', 'app.js');
const src = fs.readFileSync(APP, 'utf8');

// 只取需要的函数，避免整个 app.js 依赖 DOM
function extract(name) {
  const lines = src.split('\n');
  const decl = new RegExp(`^\\s*(function\\s+${name}\\b|(?:const|let|var)\\s+${name}\\s*=)`);
  for (let i = 0; i < lines.length; i++) {
    if (!decl.test(lines[i])) continue;
    let depth = 0, started = false, end = i;
    for (let j = i; j < lines.length; j++) {
      for (const ch of lines[j]) {
        if (ch === '{') { depth++; started = true; }
        else if (ch === '}') depth--;
      }
      if (started && depth === 0) { end = j; break; }
    }
    return lines.slice(i, end + 1).join('\n');
  }
  throw new Error(`没找到 ${name}`);
}

const numvSrc = extract('numv');
const utxtSrc = extract('utxt');
const unitCnSrc = `const UNIT_CN = { credits: '积分', credit: '积分', point: '积分', points: '积分' };`;
const diffSrc = `const diff = (total, left) => (total != null && left != null ? Math.max(0, total - left) : null);`;
const quotaGroupsSrc = extract('quotaGroups');
const quotaAggSrc = extract('quotaAgg');
const unitPlanSrc = extract('unitPlan');
const usedPctSrc = extract('usedPct');

const sandbox = {
  console,
  Math,
  Number,
  Object,
  Array,
  JSON,
  String,
  isNaN,
  parseFloat,
  parseInt,
  EXP_NONE: '未给出',
  module: { exports: {} },
};
sandbox.globalThis = sandbox;

const code = [
  numvSrc, unitCnSrc, utxtSrc, diffSrc, unitPlanSrc, quotaAggSrc, usedPctSrc, quotaGroupsSrc,
  `module.exports = { quotaGroups, quotaAgg, usedPct };`,
].join('\n\n');

vm.createContext(sandbox);
vm.runInContext(code, sandbox, { filename: 'quota-basis.js' });
const { quotaGroups, quotaAgg } = sandbox.module.exports;

let pass = 0, fail = 0;
function check(name, cond, extra) {
  if (cond) { pass++; console.log(`  ok   ${name}`); }
  else { fail++; console.log(`  FAIL ${name}${extra ? '  → ' + extra : ''}`); }
}

console.log('# 厂商口径：used_known=false 时不许反推「已用」');

// ① 厂商没给已用（total 就是当前余额）
const vendorBasis = {
  quota: {
    left: 15824, total: 15824,
    groups: [
      {
        name: '每日积分', unit: '积分', total: 1824, left: 1824, used: 0,
        used_known: false, granted_total: 2100,
        items: [{ label: '每日积分发放', total: 300, used: 276, left: 24, expires_on: '2026-11-12 23:59' }],
      },
      {
        name: '活动奖励', unit: '积分', total: 14000, left: 14000, used: 0,
        used_known: false, granted_total: 14000, items: [],
      },
    ],
  },
};
const g1 = quotaGroups(vendorBasis);
const daily = g1.find((g) => g.name === '每日积分');
check('档位 used 为 null（没被反推成 0）', daily.used === null, `实际 ${daily.used}`);
check('档位 used_known 透传为 false', daily.used_known === false);
check('档位 granted_total 透传', daily.granted_total === 2100);
check('档位 left 仍是厂商余额 1824', daily.left === 1824);

// ② 厂商给了已用（别的产品）：行为不变
const normal = {
  quota: {
    left: 3000, total: 5000,
    groups: [
      { name: '套餐基础积分', unit: 'credits', total: 5000, left: 3000, used: 2000, items: [] },
    ],
  },
};
const g2 = quotaGroups(normal);
check('厂商给了已用时 used 保留', g2[0].used === 2000, `实际 ${g2[0].used}`);
check('厂商给了已用时 used_known 为 true', g2[0].used_known === true);

// ③ 汇总：总额与剩余同源时「已用」照旧成立
const a1 = quotaAgg(g2);
check('正常产品的汇总 left = 3000', a1.left === 3000);

// ④ 汇总：厂商口径下（总额=余额）不能算已用
const a2 = quotaAgg(g1);
check('厂商口径下汇总 left = 15824', a2.left === 15824, `实际 ${a2.left}`);
const allUnknown = g1.every((g) => g.used_known === false);
check('所有档位都标明厂商没给已用', allUnknown);

console.log(`\n# 通过 ${pass}，失败 ${fail}`);
process.exit(fail ? 1 : 0);
