const test = require('node:test');
const assert = require('node:assert/strict');
const vm = require('node:vm');
const fs = require('node:fs');
const path = require('node:path');
const ctx = vm.createContext({});
vm.runInContext(fs.readFileSync(path.join(__dirname, '../ui/instance-presentation.js'), 'utf8'), ctx);

test('launch tools include installed aliases and legacy discovery records', () => {
  for (const app of [{id:'auto-agent驾驶舱'}, {name:'Agent 驾驶舱'}, {id:'custom-magpie'}, {exe:'D:/Apps/magpie-windows-amd64.exe'}, {name:'Cockpit Tools'}]) {
    assert.equal(ctx.isLaunchTool(app), true);
    assert.equal(ctx.capabilityLabel(app), '启动工具');
  }
  assert.equal(ctx.isLaunchTool({name:'Magpie Studio'}), false);
});
test('one Claude product retains runtime ids and chooses the installed runtime', () => {
  const cli = {id:'claude', installed:false}, desktop = {id:'claude-desktop', installed:true};
  const entries = ctx.productEntries([cli, {id:'other'}, desktop]);
  assert.equal(entries.length, 2);
  assert.equal(entries[1].id, desktop.id);
  assert.deepEqual([...entries[1].runtimeIds], ['claude', 'claude-desktop']);
  assert.equal(ctx.productEntries([cli, {...desktop, hidden:true}])[0], cli);
});
