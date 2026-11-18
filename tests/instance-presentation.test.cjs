const test = require('node:test');
const assert = require('node:assert/strict');
const vm = require('node:vm');
const fs = require('node:fs');
const path = require('node:path');

/* 面板要按 DOM 节点拼出来，测试里给一个够用的最小 DOM 助手 */
class Node {
  constructor(tag, attrs = {}, ...children) {
    this.tag = tag;
    this.attrs = attrs;
    this.children = children.flat().filter(x => x != null);
    this.events = {};
  }
  appendChild(child) { this.children.push(child); }
  addEventListener(name, fn) { this.events[name] = fn; }
  click() { (this.events.click || this.onclick)?.(); }
}
const make = (tag, attrs, ...kids) => new Node(tag, attrs, ...kids);
const text = node => node instanceof Node
  ? node.children.map(text).join(' ')
  : String(node ?? '');
const nodes = (node, out = []) => {
  if (!(node instanceof Node)) return out;
  out.push(node);
  node.children.forEach(child => nodes(child, out));
  return out;
};

const ctx = vm.createContext({
  el: make,
  btn: (label, cls, handler) => new Node('button', { class: cls }, label),
  stampTime: sec => `t${sec}`,
  copyText: () => {},
  invoke: () => Promise.resolve({ ok: true }),
  run: () => Promise.resolve({ ok: true }),
  load: () => {},
});
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

test('the launch tool panel reports the program and its running processes', () => {
  const app = {
    id: 'auto-magpie', name: 'Magpie', installed: true, discovered: true,
    exe: 'D:/Apps/magpie-windows-amd64.exe', exe_size: 47 * 1024 * 1024,
    exe_modified: 1700000000, first_seen: 1699000000,
  };
  const panel = ctx.launchToolPanel(app, {
    procs: [{
      app: 'Magpie', app_id: 'auto-magpie', windows: 2, count: 9,
      pids: '100,200', detail: '2 个窗口 · 9 个进程（含渲染/GPU 子进程）',
    }],
  });
  const body = text(panel);
  assert.match(body, /Magpie/);
  assert.match(body, /D:\/Apps\/magpie-windows-amd64\.exe/);
  assert.match(body, /47\.0 MB/);
  assert.match(body, /运行中（2 个窗口）/);
  assert.match(body, /9 个/);
  assert.match(body, /100,200/);
  assert.match(body, /自动发现/);
  // 面板里要留得住启动动作
  assert.ok(nodes(panel).some(node => node.tag === 'button' && text(node) === '启动程序'));
});

test('a launch tool that is not running says so instead of guessing', () => {
  const panel = ctx.launchToolPanel({
    id: 'auto-cockpittools', name: 'Cockpit Tools', installed: false, discovered: true, exe: '',
  }, { procs: [] });
  const body = text(panel);
  assert.match(body, /未运行/);
  assert.match(body, /未找到可执行文件/);
  assert.match(body, /—/);
});
