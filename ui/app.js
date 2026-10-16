/* Agent 驾驶舱 · 前端逻辑
 * 与 Rust 后端通过 Tauri invoke 通信；无内联脚本、无 innerHTML 注入（DOM 全用 el() 构建）。
 * 三个视图：账号库 / API 与额度 / 帮助。添加账号走向导，多开能力由后端实测决定。
 */
const $ = (id) => document.getElementById(id);
let invoke = () => Promise.reject(new Error('bridge 未就绪'));
let win = { setTitle() {}, hide() {}, onCloseRequested: () => Promise.resolve() };
const S = {
  inv: null,          // 最近一次「扫描登记」的结果，设置→高级 里展示
  // 落地页是首页，不是"第一个 Agent"——一进来就先看清今天要处理什么
  data: null, cur: null, view: 'home', busy: false,
  wizard: null, tour: null, detail: null, keysSoonOnly: false,
  perkForm: null,
  keysOpen: {}, keyNavSite: '',
  checkinSeen: {},    // 已经弹过提示的"积分到账"通知 id（本会话内不重复弹）
  // 签到页「产品 → 账号」两级的折叠状态：product → true 表示这家收起来了（默认全展开）
  ciFold: {},
};

/* ---------------- 外观：主题/密度/字号都来自设置页，不再占顶栏 ---------------- */
const navDrag = { active: false };
const sideDrag = { active: false };
// 侧栏正在被拖动的 Agent 的 id：非空时 renderNav 不重建列表（重建会把这次拖动掐断）
let appDrag = null;
const narrowNav = matchMedia('(max-width: 760px)');
/* 侧栏收成图标条：窗口窄是自动的，用户点过 «/» 之后就以手动为准（null = 还没手动决定） */
function railOn(a = {}) {
  const manual = a.nav_collapsed === true || a.nav_collapsed === false ? a.nav_collapsed : null;
  return manual ?? narrowNav.matches;
}
function applyRail(rail) {
  document.documentElement.classList.toggle('rail', rail);
  const b = $('btn-nav-toggle');
  if (!b) return;
  b.setAttribute('aria-expanded', String(!rail));
  b.setAttribute('aria-label', rail ? '展开导航栏' : '收起导航栏');
  b.title = rail ? '展开导航栏（Ctrl+B）' : '收起导航栏（Ctrl+B）';
}
function applyAppearance(a = {}) {
  const pref = a.theme || 'dark';
  const resolved = pref === 'system'
    ? (matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark')
    : pref;
  document.documentElement.dataset.theme = resolved;
  document.documentElement.dataset.density = a.density === 'compact' ? 'compact' : 'comfortable';
  document.body.style.zoom = String((a.font_scale || 100) / 100);
  if (!navDrag.active) {
    document.documentElement.style.setProperty('--nav-w-user', `${navWidth(a.nav_w)}px`);
  }
  if (!sideDrag.active) applySideHeight(ratioPx(sideRatio(a)));
  applyRail(railOn(a));
}
const NAV_MIN = 170, NAV_MAX = 460, NAV_DEFAULT = 248;
function navWidth(v) {
  const n = Number(v);
  return Number.isFinite(n) ? Math.round(Math.min(NAV_MAX, Math.max(NAV_MIN, n))) : NAV_DEFAULT;
}
/* 侧栏那条横线：0 = 自动（管理区正好装下它那几行，列表吃掉剩余），>0 = 用户把管理区压到多高。
   存的是**占窗口高度的比例**，不是像素——像素在最大化窗口里显得很矮、在窄窗口里又显得很高，
   而"它该占多少"本来就是比例问题（2026-10-15 她要求正常窗口和最大化窗口都按比例对应调整）。
   老版本存的是 mgmt_h 像素，第一次读到就折成比例，不用重新拖一遍。 */
const SIDE_MIN = 88;                            // 拖动时管理区最矮多少像素
const MGMT_MIN_R = 0.10, MGMT_MAX_R = 0.40;     // 比例上下限
function sideRatio(a = {}) {
  const clamp = (r) => Math.min(MGMT_MAX_R, Math.max(MGMT_MIN_R, r));
  const r = Number(a.mgmt_ratio);
  if (Number.isFinite(r) && r > 0) return clamp(r);
  const legacy = Number(a.mgmt_h);
  if (Number.isFinite(legacy) && legacy > 0) return clamp(legacy / Math.max(320, window.innerHeight));
  return 0;   // 0 = 自动：正好装下内容（CSS 里还给了个 22vh 的上限）
}
function ratioPx(r) { return r ? Math.round(r * window.innerHeight) : 0; }
function applySideHeight(px) {
  const sb = document.querySelector('.sidebar');
  document.documentElement.style.setProperty('--mgmt-h', `${px || 0}px`);
  if (sb) sb.classList.toggle('split', !!px);
}
function commitSideHeight(px) {
  const r = px > 0
    ? Math.min(MGMT_MAX_R, Math.max(MGMT_MIN_R, px / Math.max(320, window.innerHeight)))
    : 0;
  applySideHeight(r ? Math.round(r * window.innerHeight) : 0);
  const cur = sideRatio(S.data?.settings?.appearance || {});
  if (Math.abs(r - cur) > 0.005) setSetting('appearance', 'mgmt_ratio', r);
  else toast(r ? `管理区高度 ${Math.round(r * 100)}% 窗口高` : '管理区高度已恢复自动');
}
// 窗口变大变小（含最大化 / 还原）时，拖过的那个比例要跟着重算——
// 不然最大化之后管理区还是原来那点像素，占比反而变小得莫名其妙
window.addEventListener('resize', () => {
  if (sideDrag.active) return;
  applySideHeight(ratioPx(sideRatio(S.data?.settings?.appearance || {})));
});
matchMedia('(prefers-color-scheme: light)').addEventListener('change', () => {
  if (S.data) applyAppearance(S.data.settings?.appearance);
});
narrowNav.addEventListener('change', () => {
  if (S.data) applyRail(railOn(S.data.settings?.appearance));
});

/* ---------------- DOM 助手 ---------------- */
function el(tag, attrs = {}, ...kids) {
  const n = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === null || v === undefined || v === false) continue;
    if (k === 'class') n.className = v;
    else if (k === 'aria') Object.entries(v).forEach(([a, b]) => n.setAttribute(`aria-${a}`, b));
    else if (typeof v === 'object') Object.entries(v).forEach(([a, b]) => n.setAttribute(`${k}-${a}`, b));
    else n[k] = v;
  }
  for (const kid of kids.flat()) {
    if (kid === null || kid === undefined || kid === false) continue;
    n.appendChild(typeof kid === 'object' ? kid : document.createTextNode(String(kid)));
  }
  return n;
}
function btn(label, cls, handler, title) {
  const b = el('button', { class: `btn ${cls || ''}`.trim(), title }, label);
  b.addEventListener('click', handler);
  return b;
}
/* key 行的行内图标按钮（照 LongCat 的 API Keys 那种）：图标一眼认出，
   悬停有 tooltip 说清是什么；删除一直是红的。SVG 内联，不赌字体里有没有这个字形 */
const ACT_ICONS = {
  test: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M1.5 8h3l2-5.5 3 11 2-5.5h3"/></svg>',
  edit: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M10.7 2.3a1.55 1.55 0 0 1 2.2 2.2L5.5 11.9l-3 .8.8-3 7.4-7.4z"/><path d="M2.5 14.4h5.2"/></svg>',
  copy: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><rect x="5.6" y="5.6" width="7.9" height="7.9" rx="1.6"/><path d="M10.4 3.6v-.1A1.9 1.9 0 0 0 8.5 1.6H3.7a1.9 1.9 0 0 0-1.9 1.9v4.8a1.9 1.9 0 0 0 1.9 1.9h.1"/></svg>',
  plug: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M6.7 9.3l2.6-2.6"/><path d="M7.7 4.2l1.1-1.1a2.55 2.55 0 0 1 3.6 3.6l-1.1 1.1"/><path d="M8.3 11.8l-1.1 1.1a2.55 2.55 0 0 1-3.6-3.6l1.1-1.1"/></svg>',
  trash: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M2.5 4.3h11"/><path d="M6.3 4.3V3.2a1.1 1.1 0 0 1 1.1-1.1h1.2a1.1 1.1 0 0 1 1.1 1.1v1.1"/><path d="M4.2 4.3l.6 8.3a1.2 1.2 0 0 0 1.2 1.1h4a1.2 1.2 0 0 0 1.2-1.1l.6-8.3"/></svg>',
};
function iconBtn(act, handler, title) {
  const b = el('button', {
    class: `iconbtn${act === 'trash' ? ' danger' : ''}`,
    type: 'button', title, 'aria-label': (title || '').split('：')[0] || act,
  });
  b.innerHTML = ACT_ICONS[act] || act;
  b.addEventListener('click', handler);
  return b;
}
function clear(node) { while (node.firstChild) node.removeChild(node.firstChild); }

/* ---------------- 内置品牌图标 ----------------
   CLI（Codex / Claude Code）与独立客户端（ChatGPT 桌面版）没有 exe 可供后端提取图标，
   按 id / 显示名命中后直接画 SVG：24×24 品牌色圆角底 + 白色符号，深浅主题对比度各自成立。
   花结符号用 simple-icons 的 openai 原始路径（Codex 与 ChatGPT 官方都是这一个标记）。 */
const OPENAI_KNOT = 'M22.2819 9.8211a5.9847 5.9847 0 0 0-.5157-4.9108 6.0462 6.0462 0 0 0-6.5098-2.9'
  + 'A6.0651 6.0651 0 0 0 4.9807 4.1818a5.9847 5.9847 0 0 0-3.9977 2.9 6.0462 6.0462 0 0 0 .7427 7.0966'
  + ' 5.98 5.98 0 0 0 .511 4.9107 6.051 6.051 0 0 0 6.5146 2.9001A5.9847 5.9847 0 0 0 13.2599 24a6.0557'
  + ' 6.0557 0 0 0 5.7718-4.2058 5.9894 5.9894 0 0 0 3.9977-2.9001 6.0557 6.0557 0 0 0-.7475-7.0729z'
  + 'm-9.022 12.6081a4.4755 4.4755 0 0 1-2.8764-1.0408l.1419-.0804 4.7783-2.7582a.7948.7948 0 0 0'
  + ' .3927-.6813v-6.7369l2.02 1.1686a.071.071 0 0 1 .038.052v5.5826a4.504 4.504 0 0 1-4.4945 4.4944z'
  + 'm-9.6607-4.1254a4.4708 4.4708 0 0 1-.5346-3.0137l.142.0852 4.783 2.7582a.7712.7712 0 0 0 .7806'
  + ' 0l5.8428-3.3685v2.3324a.0804.0804 0 0 1-.0332.0615L9.74 19.9502a4.4992 4.4992 0 0 1-6.1408-1.6464z'
  + 'M2.3408 7.8956a4.485 4.485 0 0 1 2.3655-1.9728V11.6a.7664.7664 0 0 0 .3879.6765l5.8144 3.3543'
  + '-2.0201 1.1685a.0757.0757 0 0 1-.071 0l-4.8303-2.7865A4.504 4.504 0 0 1 2.3408 7.872zm16.5963'
  + ' 3.8558L13.1038 8.364 15.1192 7.2a.0757.0757 0 0 1 .071 0l4.8303 2.7913a4.4944 4.4944 0 0 1-.6765'
  + ' 8.1042v-5.6772a.79.79 0 0 0-.407-.667zm2.0107-3.0231l-.142-.0852-4.7735-2.7818a.7759.7759 0 0 0'
  + '-.7854 0L9.409 9.2297V6.8974a.0662.0662 0 0 1 .0284-.0615l4.8303-2.7866a4.4992 4.4992 0 0 1 6.6802'
  + ' 4.66zM8.3065 12.863l-2.02-1.1638a.0804.0804 0 0 1-.038-.0567V6.0742a4.4992 4.4992 0 0 1 7.3757'
  + '-3.4537l-.142.0805L8.704 5.459a.7948.7948 0 0 0-.3927.6813zm1.0976-2.3654l2.602-1.4998 2.6069'
  + ' 1.4998v2.9994l-2.5974 1.4997-2.6067-1.4997Z';
/* 花结画在 24 格底板上：符号占 62% 居中，四周留出品牌色的边 */
const knotTile = (bg, edge) => `<svg viewBox="0 0 24 24"><rect width="24" height="24" rx="5.5" fill="${bg}"${edge}/>`
  + `<g transform="translate(4.56 4.56) scale(.62)" fill="#fff"><path d="${OPENAI_KNOT}"/></g></svg>`;
/* Claude 星芒：官方标记（simple-icons 的 claude 路径，CC0）。底板 24 格、路径本身占满 0..24，
   按 0.59 缩放居中——与官方 app 图标实测的墨迹占比（58.8%）对齐；画到 0.66 时那 12% 的放大会
   把这个标记自带的射线不等长放大成"看着歪"。 */
const CLAUDE_BURST = 'm4.7144 15.9555 4.7174-2.6471.079-.2307-.079-.1275h-.2307l-.7893-.0486-2.6956-.0729-2.3375-.0971-2.2646-.1214-.5707-.1215-.5343-.7042.0546-.3522.4797-.3218.686.0608 1.5179.1032 2.2767.1578 1.6514.0972 2.4468.255h.3886l.0546-.1579-.1336-.0971-.1032-.0972L6.973 9.8356l-2.55-1.6879-1.3356-.9714-.7225-.4918-.3643-.4614-.1578-1.0078.6557-.7225.8803.0607.2246.0607.8925.686 1.9064 1.4754 2.4893 1.8336.3643.3035.1457-.1032.0182-.0728-.164-.2733-1.3539-2.4467-1.445-2.4893-.6435-1.032-.17-.6194c-.0607-.255-.1032-.4674-.1032-.7285L6.287.1335 6.6997 0l.9957.1336.419.3642.6192 1.4147 1.0018 2.2282 1.5543 3.0296.4553.8985.2429.8318.091.255h.1579v-.1457l.1275-1.706.2368-2.0947.2307-2.6957.0789-.7589.3764-.9107.7468-.4918.5828.2793.4797.686-.0668.4433-.2853 1.8517-.5586 2.9021-.3643 1.9429h.2125l.2429-.2429.9835-1.3053 1.6514-2.0643.7286-.8196.85-.9046.5464-.4311h1.0321l.759 1.1293-.34 1.1657-1.0625 1.3478-.8804 1.1414-1.2628 1.7-.7893 1.36.0729.1093.1882-.0183 2.8535-.607 1.5421-.2794 1.8396-.3157.8318.3886.091.3946-.3278.8075-1.967.4857-2.3072.4614-3.4364.8136-.0425.0304.0486.0607 1.5482.1457.6618.0364h1.621l3.0175.2247.7892.522.4736.6376-.079.4857-1.2142.6193-1.6393-.3886-3.825-.9107-1.3113-.3279h-.1822v.1093l1.0929 1.0686 2.0035 1.8092 2.5075 2.3314.1275.5768-.3218.4554-.34-.0486-2.2039-1.6575-.85-.7468-1.9246-1.621h-.1275v.17l.4432.6496 2.3436 3.5214.1214 1.0807-.17.3521-.6071.2125-.6679-.1214-1.3721-1.9246L14.38 17.959l-1.1414-1.9428-.1397.079-.674 7.2552-.3156.3703-.7286.2793-.6071-.4614-.3218-.7468.3218-1.4753.3886-1.9246.3157-1.53.2853-1.9004.17-.6314-.0121-.0425-.1397.0182-1.4328 1.9672-2.1796 2.9446-1.7243 1.8456-.4128.164-.7164-.3704.0667-.6618.4008-.5889 2.386-3.0357 1.4389-1.882.929-1.0868-.0062-.1579h-.0546l-6.3385 4.1164-1.1293.1457-.4857-.4554.0608-.7467.2307-.2429 1.9064-1.3114Z';
const burstTile = (bg) => `<svg viewBox="0 0 24 24"><rect width="24" height="24" rx="5.5" fill="${bg}"/>`
  + `<g transform="translate(4.92 4.92) scale(.59)" fill="#F0EEE6"><path d="${CLAUDE_BURST}"/></g></svg>`;
const BUILTIN_ICONS = [
  // Codex CLI：OpenAI 绿底 + 白色花结
  [/codex/i, knotTile('#10A37F', '')],
  // Claude Code：赭橙底 + 白色星芒
  [/claude/i, burstTile('#D97757')],
  // ChatGPT / OpenAI：黑底 + 白色花结，深色主题下描一圈边免得和底融了
  [/chatgpt|chat gpt|openai/i, knotTile('#111', ' stroke="rgba(128,128,128,.45)"')],
];
function builtinIcon(a) {
  const key = `${a.id} ${a.name}`;
  for (const [re, svg] of BUILTIN_ICONS) if (re.test(key)) return svg;
  return '';
}

/* ---------------- 行内问号提示 ----------------
   设置项的长说明从行里收进 label 旁的问号圈：悬停即看、单击钉住（再点/点别处/Esc/滚动收掉）。
   气泡 fixed 挂 body 手动定位——设置面板与浮窗都是 overflow 滚动容器，absolute 会被裁。 */
const DESC_INLINE_MAX = 26;   // 超过这个长度就从行内收进问号（行内只留一眼能看完的）
let qtipEl = null;
let qtipAnchor = null;        // 被单击钉住的那个问号；null = 当前只是悬停态
function qtipShow(anchor, text, pin) {
  qtipHide();
  if (pin) qtipAnchor = anchor;
  qtipEl = el('div', { id: 'qtip', role: 'tooltip' }, text);
  document.body.appendChild(qtipEl);
  const r = anchor.getBoundingClientRect();
  const w = qtipEl.offsetWidth;
  const h = qtipEl.offsetHeight;
  const x = Math.max(8, Math.min(r.left, window.innerWidth - w - 12));
  let y = r.bottom + 8;
  if (y + h > window.innerHeight - 8) y = Math.max(8, r.top - h - 8);
  qtipEl.style.left = `${x}px`;
  qtipEl.style.top = `${y}px`;
  if (pin) anchor.classList.add('on');
}
function qtipHide() {
  if (qtipAnchor) qtipAnchor.classList.remove('on');
  qtipAnchor = null;
  if (qtipEl) { qtipEl.remove(); qtipEl = null; }
}
function qhelp(text) {
  const s = el('span', { class: 'qhelp', role: 'note', tabindex: '0', aria: { label: `说明：${text}` } }, '?');
  s.addEventListener('mouseenter', () => { if (qtipAnchor !== s) qtipShow(s, text, false); });
  s.addEventListener('mouseleave', () => { if (!qtipAnchor) qtipHide(); });
  s.addEventListener('click', (e) => {
    e.stopPropagation();
    if (qtipAnchor === s) qtipHide();
    else qtipShow(s, text, true);
  });
  return s;
}
// 钉住后：点别处 / Esc / 任意滚动（scroll 不冒泡，用捕获阶段）都收掉
document.addEventListener('click', (e) => {
  if (qtipAnchor && !qtipAnchor.contains(e.target)) qtipHide();
});
document.addEventListener('keydown', (e) => { if (e.key === 'Escape') qtipHide(); }, true);
document.addEventListener('scroll', () => qtipHide(), true);
function chip(text, cls) { return el('span', { class: `tag ${cls || ''}`.trim() }, text); }
function fmtTok(n) {
  n = Number(n);
  if (!Number.isFinite(n)) return '—';
  if (n >= 1e9) return (n / 1e9).toFixed(2) + 'B';
  if (n >= 1e6) return (n / 1e6).toFixed(2) + 'M';
  if (n >= 1e3) return (n / 1e3).toFixed(1) + 'K';
  return String(Math.round(n));
}

/* 积分数字：千分位 + 最多两位小数。
   接口给的是 2694.84000019 这种浮点尾巴，原样上界面就是"显示没改好"——先四舍五入到分，
   再借 toLocaleString 去掉无意义的 .00。 */
function nfmt(v) {
  const n = Number(v);
  if (v === null || v === undefined || v === '' || !Number.isFinite(n)) return '—';
  const r = Math.round(n * 100) / 100;
  return r.toLocaleString('en-US', { maximumFractionDigits: 2 });
}

/* 日期时间：全站只认这一套写法 YYYY-MM-DD HH:MM。
   原生 <input type=date> 在中文系统上渲染成 "yyyy/mm/日"（中英杂糅），一律不再用。 */
const EXP_NONE = '未给出';
function dtxt(s) {
  if (s === null || s === undefined || s === '') return '—';
  const t = String(s).trim();
  if (t === EXP_NONE || t === '无限期' || t === '—') return t;
  // "2026/10/11T15:31:36+08:00" / "2026-10-11 15:31:36" / epoch 秒 都收进来重排
  if (typeof t === 'string' && /^\d{9,11}$/.test(t)) return dtxt(new Date(Number(t) * 1000).toISOString());
  const m = t.match(/^(\d{4})-(\d{2})-(\d{2})[T ](\d{2}):(\d{2})/);
  if (m) return `${m[1]}-${m[2]}-${m[3]} ${m[4]}:${m[5]}`;
  const d = t.match(/^(\d{4})-(\d{2})-(\d{2})$/);
  if (d) return t;
  const p = Date.parse(t);
  if (!Number.isNaN(p)) {
    const x = new Date(p);
    const z = (v) => String(v).padStart(2, '0');
    return `${x.getFullYear()}-${z(x.getMonth() + 1)}-${z(x.getDate())} ${z(x.getHours())}:${z(x.getMinutes())}`;
  }
  return t;
}

/* 剩余额度对应的到期话术：套餐基础积分是"下次刷新"，赠送的那类是"最近到期" */
function expiryLabel(group) {
  return /基础|订阅|套餐/.test(group.name || '') && !/奖励|赠送/.test(group.name || '')
    ? '下次刷新' : '最近到期';
}

/* ---------------- 轻提示 ---------------- */
function toast(msg, ok = true) {
  const t = el('div', { class: ok ? '' : 'err' }, msg);
  $('toast').appendChild(t);
  setTimeout(() => t.remove(), 5200);
}

/* 发布版没有 devtools：渲染出错要当场写在页面上，否则只剩一片空白让人猜 */
window.addEventListener('error', (e) => {
  S.uiError = `${e.message} @ ${(e.filename || '').split('/').pop()}:${e.lineno}`;
});

/* ---------------- 页面内确认框 ---------------- */
/* 绝不用原生 confirm/prompt：主窗口 decorations:false，WebView2 会把脚本对话框
   弹成另一个顶层窗口，鼠标点不动（实测只有 Enter/Esc 有效）。 */
let askCb = null;
function askDone(val) {
  const m = $('ask');
  if (m.hidden) return;
  m.hidden = true;
  document.removeEventListener('keydown', askKey, true);
  const cb = askCb;
  askCb = null;
  if (cb) cb(val);
}
function askSubmit() {
  const inp = $('ask-input');
  askDone(inp.hidden ? true : (inp.value.trim() || null));
}
function askKey(e) {
  if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); askDone(null); }
  else if (e.key === 'Enter') { e.preventDefault(); e.stopPropagation(); askSubmit(); }
}
// resolve: true / 输入框内容 / null（取消）
function ask({ msg = '', title = '请确认', okText = '确定', danger = false, input = null } = {}) {
  return new Promise((resolve) => {
    askCb = resolve;
    $('ask-title').textContent = title;
    $('ask-body').textContent = msg;
    $('ask').classList.toggle('danger', danger);   // 危险操作：标题带红点、确定钮实心红
    const inp = $('ask-input');
    inp.hidden = input === null;
    inp.value = input === null ? '' : input;
    const ok = $('ask-ok');
    ok.textContent = okText;
    ok.className = `btn ${danger ? 'danger' : 'primary'}`;
    $('ask').hidden = false;
    (inp.hidden ? ok : inp).focus();
    document.addEventListener('keydown', askKey, true);
  });
}
const askConfirm = (msg, opts = {}) => ask({ msg, ...opts }).then((v) => v !== null);
const askText = (msg, def = '', opts = {}) => ask({ msg, input: def, ...opts });

/* ---------------- 后端调用 ---------------- */
async function run(action, extra = {}) {
  const r = await invoke('action', { payload: { action, ...extra } });
  toast(r.msg, r.ok);
  load();
  return r;
}
async function load() {
  if (S.busy) return;
  S.busy = true;
  try {
    S.data = await invoke('get_state');
    if (!S.cur || !S.data.apps.some((a) => a.id === S.cur)) S.cur = S.data.apps[0]?.id;
    render();
  } catch (e) {
    toast(`状态读取失败：${e}`, false);
  } finally {
    S.busy = false;
  }
}

/* ---------------- 渲染总入口 ---------------- */
/* 光标停在输入框里就别重建那一块：4 秒轮询会把还没提交的输入抹回后端的值。
   在剪贴板里翻找一段文字回来，框里的东西没了——就是这么来的。 */
function editingIn(sel) {
  const a = document.activeElement;
  const host = document.querySelector(sel);
  return !!(a && host && host.contains(a) && /^(INPUT|TEXTAREA|SELECT)$/.test(a.tagName));
}

// 账号库整页重建会跳回页顶，记着"同一个产品"才把滚动位置还回去
let agentsKey = '';

function render() {
  const d = S.data;
  applyAppearance(d.settings?.appearance);
  renderChips(d);
  renderAlerts(d);
  renderNav(d);
  renderViews();
  // 签到状态：整份缓存由 4 秒一轮的 state 带回来。这一页自己那份 S.checkin 只用来
  // 触发刷新，显示一律以 state 里更新鲜的那份为准——否则页面会停在进来那一刻的旧数上。
  const dc = d.checkin || {};
  if (dc.ok && (dc.fetched_at || 0) >= ((S.checkin || {}).fetched_at || 0)) S.checkin = dc;
  notifyArrivals(dc);
  // 设置 / API / 协作 是整窗页面：盖掉侧栏，页面自己带左菜单，右侧内容才有地方放。
  // 首页不盖侧栏——它就是"从左边挑一个 Agent"的入口。
  document.querySelector('.layout').classList.toggle('full', S.view !== 'agents' && S.view !== 'home');
  $('btn-settings').setAttribute('aria-current', String(S.view === 'settings'));
  if (S.view === 'home') renderHome(d);
  if (S.view === 'agents') {
    // 卡片/福利/进程这三块每轮都整块重建：清空的那一瞬间 scrollHeight 没了，
    // 浏览器把 scrollTop 夹回 0——看起来就是"点一下按钮，整个页面跳回最上面"（4 秒轮询同样会跳）
    const pb = document.querySelector('#view-agents .pagebody');
    const keep = pb ? pb.scrollTop : 0;
    const sameApp = agentsKey === `${S.view}|${S.cur}`;
    agentsKey = `${S.view}|${S.cur}`;
    renderToolbar(d); renderCards(d); renderPerks(d); renderProcs(d); renderProxy(d);
    if (pb) pb.scrollTop = sameApp ? keep : 0;
  }
  if (S.view === 'keys') renderKeys(d);
  if (S.view === 'checkin') { renderCheckin(); loadCheckin(false); }
  if (S.view === 'settings' && !editingIn('#set-body')) renderSettings();
  // 账号设置浮窗开着时跟着轮询刷新状态；有未保存改动或光标还在框里就不重绘
  if (S.detail && !S.detail.dirty && !editingIn('#im-body')) renderDetail();
  // 积分明细弹层：只在真的又读了一次数之后重绘。每 4 秒重建 60 多行会把滚动位置顶回顶部
  if (S.qdetail && !pendingQuota) {
    const x = (S.data.instances || []).find((y) => y.app === S.qdetail.app && y.name === S.qdetail.name);
    if (!x || x.quota.fetched_at !== S.qdetail.at) renderQuotaDetail();
  }
}

function renderChips(d) {
  const ac = $('c-agents');
  ac.querySelector('span').textContent = `Agent ${d.stats.agents}`;
  ac.title = d.scan && d.scan.at
    ? `其中 ${d.stats.auto} 个由扫描自动发现 · 上次扫描 ${d.scan.at}`
    : `其中 ${d.stats.auto} 个由扫描自动发现`;
  $('c-inst').querySelector('span').textContent = `账号 ${d.stats.instances}`;
  const runChip = $('c-run');
  runChip.querySelector('span').textContent = `运行中 ${d.stats.running}`;
  runChip.title = `${d.stats.running} 个 Agent 程序在跑（其中 ${d.stats.running_instances} 个是本工具的独立实例）`;
  runChip.classList.toggle('on', d.stats.running > 0);
  const pc = $('c-proxy');
  pc.querySelector('span').textContent = d.proxy.running ? `反代 :${d.proxy.port}` : '反代 停止';
  pc.classList.toggle('on', d.proxy.running);
  $('c-fresh').textContent = d.now || '';
  // 顶栏签到角标：只在"今天还有得领"时才出现，全领完或一家都不支持时不占位
  const cc = $('c-checkin');
  const pend = (((d.checkin || {}).items) || []).filter((x) => x.claimable === true);
  cc.hidden = !pend.length;
  cc.classList.toggle('expiring', pend.length > 0);
  cc.querySelector('span').textContent = `签到 ${pend.length}`;
  cc.title = `今天还没领：${pend.map((x) => x.name || x.product).join('、')}\n点开「每日签到」看状态或直接领`;
}

// 临期提醒不再占窗口空间：顶栏只留一个小徽章，真正快过期时弹一次窗。
function renderAlerts(d) {
  const chip = $('c-alerts');
  const danger = d.alerts.some((a) => a.level === 'danger');
  if (!d.alerts.length) {
    chip.hidden = true;
    win.setTitle('Agent 驾驶舱');
    return;
  }
  chip.hidden = false;
  chip.classList.toggle('danger', danger);
  chip.querySelector('span').textContent = `临期 ${d.alerts.length}`;
  win.setTitle(`(${d.alerts.length} 笔临期) Agent 驾驶舱`);
  warnExpiring(d.alerts);
}

function expiringText(alerts) {
  return alerts.map((a) => `「${a.app}/${a.name}」${a.label ? ` · ${a.label}` : ''}`
    + ` 剩余 ${nfmt(a.left)}${a.unit ? ` ${utxt(a.unit)}` : ''} · ${expText(a.days)}`).join('\n');
}

let expWarnedOn = null;   // 本次会话已弹过就不重复弹
// 只对真正紧迫的（7 天内）弹窗提醒，且同一天最多弹一次——它是提醒，不该天天挡路
async function warnExpiring(alerts) {
  const urgent = alerts.filter((a) => (a.days ?? 999) <= 7);
  if (!urgent.length) return;
  const today = new Date().toISOString().slice(0, 10);
  if (expWarnedOn === today) return;
  expWarnedOn = today;
  const KEY = 'ac.expiringWarnedOn';
  if (localStorage.getItem(KEY) === today) return;
  localStorage.setItem(KEY, today);
  await ask({
    title: `⏳ ${urgent.length} 笔积分 7 天内到期`,
    msg: `${expiringText(urgent)}\n\n过期作废，记得尽快用掉。`,
    okText: '知道了',
  });
}

async function showAlerts() {
  const alerts = (S.data && S.data.alerts) || [];
  if (!alerts.length) { toast('暂无临期积分'); return; }
  await ask({ title: `⏳ ${alerts.length} 笔积分临期`, msg: expiringText(alerts), okText: '知道了' });
}

function renderNav(d) {
  // 拖动排序进行中别重建列表：4 秒轮询正好撞在拖拽半路上会让这次拖动失效
  if (appDrag) return;
  const ul = $('applist');
  const box = ul.closest('.side-scroll');
  const keep = box ? box.scrollTop : 0;   // 重建列表会让滚动条弹回顶部，先记住
  clear(ul);
  for (const a of d.apps) {
    const n = d.instances.filter((i) => i.app === a.id).length;
    // 只有"有窗口"才算这个程序开着：后台驻留的进程不该让侧栏亮绿灯
    const running = d.instances.some((i) => i.app === a.id && i.running)
      || d.procs.some((p) => p.app_id === a.id && (p.windows ?? 1) > 0);
    const cap = a.kind === 'cli' ? '终端' : (a.isolatable ? '可多开' : '仅换号');
    // 「现在用的是哪个号」：共用一份档案的产品以客户端登录态为准（磁盘换号记录不随登录变化）；
    // 只有这个产品压根没有本机直读通道、登录态读不到时，才退回按档案认领关系标。
    const mv = (a.machine && a.machine.value) || {};
    const loggedName = a.shared_profile
      ? (d.instances.find((x) => x.app === a.id && x.logged_in)?.name
          || (mv.ok ? mv.account || '' : ''))
      : '';
    const cur = a.shared_profile
      ? (loggedName ? { name: loggedName, tag: '本机登录中' }
        : (!a.local_quota ? { name: a.swapped, tag: '当前账号' } : { name: '', tag: '' }))
      : { name: a.swapped, tag: '当前账号' };
    // 图标优先级：内置品牌 SVG > 后端从 exe 提取的 PNG > 名字缩写占位。
    // 内置的排第一是因为后端对没有图标的 exe 会拿到 Windows 那张"空白文档"默认图，
    // 认不全的品牌才轮到它。
    const bisvg = builtinIcon(a);
    const icon = bisvg
      ? el('span', { class: 'fav svg', title: a.name })
      : a.icon
        ? el('img', { class: 'fav', src: a.icon, alt: '', width: 18, height: 18 })
        : el('span', { class: 'fav none' }, (a.short || a.name[0]).slice(0, 2));
    if (bisvg) icon.innerHTML = bisvg;
    const b = el('button', {
      class: `nav-btn${running ? ' running' : ''}${a.installed ? '' : ' missing'}`,
      data: { app: a.id },
      aria: { current: S.view === 'agents' && a.id === S.cur },
      title: `${a.name}\n`
        + `${a.installed ? '已安装' : (a.removed ? '已卸载（可执行文件不存在）' : '未找到可执行文件')} · ${cap}`
        + (cur.name ? `\n${cur.tag}：${cur.name}` : '')
        + '\n按住拖动可上下排位（右键也有 置顶/上移/下移）',
    },
      el('span', { class: 'dot' }),
      icon,
      el('span', { class: 'label' }, a.name),
      n ? el('span', { class: 'badge' }, n) : null,
      el('span', { class: 'nav-badges' },
        (cur.name || a.swap_orphan) ? el('span', {
          class: `badge swap${a.swap_orphan ? ' orphan' : ''}`,
          title: a.swap_orphan
            ? `档案目录还指向已删除的账号「${a.swapped}」，点进该程序用「还原默认档案」修好`
            : `${cur.tag}：${cur.name}`,
        }, `⇄${cur.name || a.swapped}`) : null,
        a.new ? el('span', { class: 'badge new', title: '最近新收录的程序' }, '新') : null,
        a.removed ? el('span', { class: 'badge off', title: '可执行文件已不存在，可能已卸载' }, '已卸载') : null,
        el('span', { class: `cap ${a.isolatable ? 'yes' : (a.kind === 'cli' ? 'cli' : 'no')}`, title: cap },
          a.isolatable ? '多开' : (a.kind === 'cli' ? '终端' : '换号'))));
    b.addEventListener('click', () => {
      if (dragMoved) { dragMoved = false; return; }   // 刚拖完别顺手选中这一项
      S.view = 'agents';
      S.cur = a.id;
      if (a.new) {
        // 点开看过一次就摘掉「新」：先就地改内存（立刻消失），再让后端记下
        a.new = false;
        invoke('action', { payload: { action: 'mark_seen', app: a.id } })
          .catch(() => { a.new = true; });
      }
      render();
    });
    bindNavSort(b, a.id);
    ul.appendChild(el('li', {}, b));
  }
  document.querySelectorAll('#viewlist [data-view]').forEach((x) => {
    x.setAttribute('aria-current', String(S.view === x.dataset.view));
  });
  if (box) box.scrollTop = keep;
}

/* 侧栏拖动排序：按住某一项往上/下拖，亮线表示"插在这一项上面/下面"，松手先就地重排
   （马上看得见），再把整份次序交给后端存进 settings.app_order。
   用 pointer 事件而不是 HTML5 draggable：本应用侧栏宽度分隔线就是这套、实测可靠，
   而 HTML5 拖拽经常 dragstart 根本不触发（合成输入、部分内核尤其明显）。 */
let dragMoved = false;
function bindNavSort(b, id) {
  // 图标是 <img>：不拦原生 dragstart 的话，按住图标拖动会变成"拖图片"，pointerup 就丢了
  b.addEventListener('dragstart', (e) => e.preventDefault());
  b.addEventListener('pointerdown', (e) => {
    if (e.button !== 0 || e.pointerType === 'touch') return;   // 触摸留给滚动
    const y0 = e.clientY;
    let on = false, hit = null;
    const mark = (t, after) => {
      document.querySelectorAll('#applist .nav-btn')
        .forEach((x) => { if (x !== t) x.classList.remove('drop-above', 'drop-below'); });
      if (!t) return;
      t.classList.toggle('drop-above', !after);
      t.classList.toggle('drop-below', after);
    };
    const move = (ev) => {
      if (!on) {
        if (Math.abs(ev.clientY - y0) < 5) return;   // 5px 以内算点击时的手抖
        on = true;
        appDrag = id;
        b.classList.add('dragging');
      }
      const under = document.elementFromPoint(ev.clientX, ev.clientY);
      const t = under && under.closest('#applist .nav-btn');
      if (!t || t === b) { hit = null; mark(null); return; }
      const r = t.getBoundingClientRect();
      const after = ev.clientY > r.top + r.height / 2;
      hit = { id: t.dataset.app, after };
      mark(t, after);
    };
    const up = () => {
      document.removeEventListener('pointermove', move, true);
      document.removeEventListener('pointerup', up, true);
      document.removeEventListener('pointercancel', up, true);
      b.classList.remove('dragging');
      mark(null);
      if (!on) return;
      dragMoved = true;          // 松手落在原处时别让 click 把它当成"选中"
      appDrag = null;            // 先解锁，否则 applyNavSort 里的 render() 会被 renderNav 跳过
      if (hit && hit.id && hit.id !== id) applyNavSort(id, hit.id, hit.after);
      else render();
    };
    document.addEventListener('pointermove', move, true);
    document.addEventListener('pointerup', up, true);
    document.addEventListener('pointercancel', up, true);
  });
}

function applyNavSort(fromId, toId, after) {
  const apps = ((S.data && S.data.apps) || []).slice();
  const i = apps.findIndex((a) => a.id === fromId);
  if (i < 0) return;
  const moved = apps.splice(i, 1)[0];
  const j = apps.findIndex((a) => a.id === toId);
  if (j < 0) return;
  apps.splice(after ? j + 1 : j, 0, moved);
  S.data.apps = apps;
  render();
  invoke('action', { payload: { action: 'app_reorder', app: fromId, ids: apps.map((a) => a.id) } })
    .then((r) => { if (!r.ok) { toast(r.msg, false); load(); } });
}

/* 视图切换只换右栏内容：每页自己分「左菜单 + 右内容」，标题栏固定不滚 */
function renderViews() {
  for (const v of ['home', 'agents', 'collab', 'keys', 'checkin', 'settings']) {
    $(`view-${v}`).hidden = S.view !== v;
  }
}

/* ---------------- 首页（落地页） ----------------
   一进来先回答三个问题：这台机器上有什么、今天要处理什么、现在能一键做什么。
   两个主操作是"一键刷新全部额度"和"一键签到"——以前它们散在各个页面里，
   要一家一家点。 */
function homeGo(appId) {
  S.view = 'agents';
  if (appId) S.cur = appId;
  render();
}

/* 到账提示：后台把"待观察"的那几家读回来，到账了就写进 checkin.arrived。
   这里认新的 id 弹一条——她说的"下方操作提示区"就是这排 toast。 */
function notifyArrivals(dc) {
  for (const n of (dc && dc.arrived) || []) {
    const id = n.id || '';
    if (!id || S.checkinSeen[id]) continue;
    S.checkinSeen[id] = true;
    toast(n.text || '有积分到账了', true);
  }
}

function homeStat(label, value, sub, cls) {
  return el('div', { class: `hstat${cls ? ` ${cls}` : ''}` },
    el('span', { class: 'hstat-l' }, label),
    el('b', { class: 'hstat-v' }, value),
    sub ? el('span', { class: 'hstat-s' }, sub) : null);
}

function renderHome(d) {
  const host = $('home-body');
  if (!host) return;
  const pb = host.closest('.pagebody');
  const keep = pb ? pb.scrollTop : 0;
  clear(host);
  $('home-sub').textContent = `${d.stats.agents} 个程序 · ${d.stats.instances} 个账号`
    + (d.now ? ` · ${d.now}` : '');

  const chk = d.checkin || {};
  const items = chk.items || [];
  const claimable = items.filter((x) => x.claimable === true && !x.error);
  const signed = items.filter((x) => x.signed_today === true);
  const alerts = d.alerts || [];
  const urgent = alerts.filter((a) => (a.days ?? 999) <= 7);
  const job = d.alljob || null;
  const running = job && job.running;

  // ① 今天的待办 + 两个主操作
  const todo = [];
  if (chk.placeholder) todo.push('签到状态正在读（列表已经先摆出来了，状态一两秒内补上）');
  if (claimable.length) todo.push(`${claimable.length} 家今天还能领：${claimable.map((x) => x.name || x.product).join('、')}`);
  else if (!chk.placeholder && items.length) todo.push('今天的签到都领完了');
  if (urgent.length) todo.push(`${urgent.length} 笔积分 7 天内到期`);
  if (d.stats.running) todo.push(`${d.stats.running} 个程序正在跑`);

  const hero = el('div', { class: 'hhero' },
    el('div', { class: 'hhero-t' },
      el('h2', {}, '今天要处理的事'),
      el('p', { class: 'hhero-sub' }, todo.length ? todo.join(' · ') : '暂时没有要处理的，都挺干净')),
    el('div', { class: 'hhero-b' },
      btn(running ? '↻ 正在刷新…' : '↻ 一键刷新全部额度', 'primary lg', () => refreshAllQuota(),
        '把每个 Agent 每个账号的额度读一遍：能走官网的读逐包明细与各自到期时间，读不到就退本机/服务端汇总。'
        + '这一轮在后台跑，进度就在按钮下面。'),
      btn('✓ 一键签到', 'primary lg', () => claimAllCheckin(),
        '把今天所有能领的一次领掉——领之前会先让你确认一遍清单。领走的当日名额退不回来，所以还是要你点一下。')));
  if (job) {
    const pct = job.total ? Math.round((job.done / job.total) * 100) : 0;
    hero.appendChild(el('div', { class: 'hjob' },
      el('span', { class: 'hjob-t' },
        running
          ? `正在读 ${job.done}/${job.total}${job.current ? `：${job.current}` : ''}`
          : `上次刷新：${job.total} 个账号，成功 ${job.ok} / 失败 ${job.fail}`
            + (job.finished_at ? ` · ${stampTime(job.finished_at)}` : '')),
      running ? el('div', { class: 'hbar' }, el('i', { style: `width:${pct}%` })) : null,
      !running && job.log && job.log.length
        ? el('div', { class: 'hjoblog' }, ...job.log.filter((x) => !x.ok).slice(0, 4)
            .map((x) => el('div', { class: 'hjobbad' }, `${x.app} / ${x.name}：${x.msg || '没读到'}`)))
        : null));
  }
  host.appendChild(hero);

  // ② 统计磁贴
  const totalLeft = (d.instances || []).reduce((s, i) => {
    const v = numv(i.quota && i.quota.left);
    return s + (v && v > 0 ? v : 0);
  }, 0);
  host.appendChild(el('div', { class: 'hstats' },
    homeStat('Agent 程序', String(d.stats.agents), `${d.stats.auto} 个是自动发现的`),
    homeStat('账号', String(d.stats.instances), `${d.stats.running} 个程序正在跑`),
    homeStat('账上剩余', nfmt(totalLeft), '所有账号的可读积分合计'),
    homeStat('临期积分', String(alerts.length), urgent.length ? `其中 ${urgent.length} 笔 7 天内到期` : '30 天内到期的笔数',
      alerts.length ? 'warn' : ''),
    homeStat('今日签到', `${signed.length}/${items.length || '—'}`,
      chk.placeholder ? '正在读状态' : (claimable.length ? `${claimable.length} 家还能领` : '都领完了'),
      claimable.length ? 'go' : '')));

  // ③ 两栏：临期积分 / 今天还能领
  const cols = el('div', { class: 'hcols' });
  const acard = el('section', { class: 'panel' },
    el('header', { class: 'panelhead' }, el('h2', {}, '临期积分'),
      el('span', { class: 'sub' }, alerts.length ? '点一行进它的账号页' : '30 天内没有到期的')),
    el('div', { class: 'hlist' },
      ...(alerts.length
        ? alerts.slice(0, 8).map((a) => el('button', {
            class: `hrow${(a.days ?? 999) <= 7 ? ' urgent' : ''}`, type: 'button',
            onclick: () => homeGo(a.app_id),
          },
            el('span', { class: 'hrow-l' }, `${a.app} · ${a.name}${a.label ? ` · ${a.label}` : ''}`),
            el('span', { class: 'hrow-r' }, `剩 ${nfmt(a.left)}${a.unit ? ` ${utxt(a.unit)}` : ''}`),
            el('span', { class: `hrow-x ${expCls(a.days, false)}` }, expText(a.days))))
        : [el('p', { class: 'qempty' }, '没有临期的积分。')])));
  const ccard = el('section', { class: 'panel' },
    el('header', { class: 'panelhead' }, el('h2', {}, '今天还能领的'),
      el('span', { class: 'sub' }, chk.fetched_at ? `状态查于 ${stampTime(chk.fetched_at)}` : '还没查到')),
    el('div', { class: 'hlist' },
      ...(claimable.length
        ? claimable.map((x) => el('button', {
            class: 'hrow', type: 'button',
            onclick: () => { S.view = 'checkin'; render(); },
          },
            el('span', { class: 'hrow-l' }, x.name || x.product),
            el('span', { class: 'hrow-r' }, x.reward != null ? `${x.reward} ${x.unit || '积分'}` : '—'),
            el('span', { class: 'hrow-x go' }, ciActionWord(x))))
        : [el('p', { class: 'qempty' }, chk.placeholder ? '正在问各家今天的状态…' : '今天没有可领的了。')])));
  cols.appendChild(acard);
  cols.appendChild(ccard);
  host.appendChild(cols);

  // ④ 我的 Agent 一行一个
  // 每一项都要自己包一层 span：flex 容器里连着几段裸文本会被合成**一个**匿名项，
  // 中间的 gap 就不生效了（第一版就是这样，"运行中2 个账号可多开"糊成一团）
  const ciWord = { done: '已签', claimable: '可领', off: '活动未开', error: '读不到' };
  const rows = (d.apps || []).filter((a) => !a.hidden).map((a) => {
    const insts = (d.instances || []).filter((i) => i.app === a.id);
    const mv = (a.machine && a.machine.value) || {};
    const logged = a.shared_profile
      ? (insts.find((x) => x.logged_in)?.name || (mv.ok ? mv.account || '' : ''))
      : (a.swapped || '');
    const run = insts.some((i) => i.running) || (d.procs || []).some((p) => p.app_id === a.id && (p.windows ?? 1) > 0);
    const aAlerts = alerts.filter((x) => x.app_id === a.id).length;
    const ci = insts.map((i) => i.checkin).find((c) => c && c.state);
    const tags = [
      run ? '运行中' : '未运行',
      insts.length ? `${insts.length} 个账号` : '还没有账号',
      a.isolatable ? '可多开' : (a.shared_profile ? '共用一份档案' : '仅可换号'),
      logged ? `当前：${logged}` : '',
      aAlerts ? `${aAlerts} 笔临期` : '',
      ci ? `签到：${ciWord[ci.state] || '状态未知'}${ci.reward != null ? ` · 每日 ${ci.reward}` : ''}` : '',
      a.installed ? '' : '可执行文件已不存在',
    ].filter(Boolean);
    return el('button', { class: 'hagent', type: 'button', onclick: () => homeGo(a.id) },
      el('span', { class: 'hagent-n' }, a.name),
      el('span', { class: 'hagent-t' }, ...tags.map((t) => el('span', {}, t))));
  });
  host.appendChild(el('section', { class: 'panel' },
    el('header', { class: 'panelhead' }, el('h2', {}, '我的 Agent'),
      el('span', { class: 'sub' }, '点一行进它的账号页')),
    el('div', { class: 'hlist' }, ...(rows.length ? rows : [el('p', { class: 'qempty' }, '还没有收录任何程序。')]))));

  // ⑤ 最近领过的
  const log = (((chk.state || {}).log) || []).slice(-6).reverse();
  if (log.length) {
    host.appendChild(el('section', { class: 'panel' },
      el('header', { class: 'panelhead' }, el('h2', {}, '最近领过的'),
        el('span', { class: 'sub' }, '只记本机动作，不联网')),
      el('div', { class: 'hlog' }, ...log.map((x) => el('div', { class: 'hlogrow' },
        el('span', { class: 'hlog-t' }, `${x.at || ''} ${CI_NAMES[x.store] || x.store}`),
        el('span', { class: x.claimed ? 'hlog-ok' : 'hlog-no' },
          x.claimed ? `领到 ${x.credit != null ? x.credit : ''}`.trim()
            : x.already ? '已领过' : `没领成：${x.message || x.error || ''}`))))));
  }

  if (pb) pb.scrollTop = keep;
}

/* 一键刷新全部额度：后端起后台任务，这里只管点火 + 提示，进度靠 4 秒轮询自己长出来 */
async function refreshAllQuota() {
  const r = await invoke('action', { payload: { action: 'refresh_all' } })
    .catch((e) => ({ ok: false, msg: String(e) }));
  toast((r && r.msg) || '没起起来', !!(r && r.ok));
  load();
}

/* 一键签到：先把今天能领的摊出来让她点头，再一家一家领。
   领是写操作，所以即使叫"一键"也必须有这一下确认。 */
async function claimAllCheckin() {
  let items = ((S.checkin || {}).items || []).filter((x) => x.claimable === true && !x.error);
  if (!items.length) {
    // 手上这份可能是旧的：先强制读一次，再决定要不要领
    const r = await invoke('action', { payload: { action: 'checkin_refresh' } }).catch(() => null);
    if (r && r.ok) { S.checkin = r; items = (r.items || []).filter((x) => x.claimable === true && !x.error); }
  }
  if (!items.length) { toast('今天没有可领的了', true); load(); return; }
  const lines = items.map((x) => `· ${x.name || x.product}${x.reward != null ? `（${x.reward} ${x.unit || '积分'}）` : ''}`);
  const yes = await askConfirm(
    `今天这 ${items.length} 家一起领掉？\n\n${lines.join('\n')}\n\n`
    + '领走的当日名额退不回来。逐条结果会写进「最近领过的」。',
    { title: '一键签到', okText: `领这 ${items.length} 家` });
  if (!yes) return;
  let ok = 0, no = 0;
  const bad = [];
  for (const x of items) {
    const r = await invoke('action', { payload: { action: 'checkin_claim', app: '', store: x.product } })
      .catch((e) => ({ ok: false, msg: String(e) }));
    const y = (r && r.result) || {};
    if (y.claimed || y.already || y.settling) ok += 1;
    else { no += 1; bad.push(`${x.name || x.product}：${y.message || y.error || (r && r.msg) || '没领成'}`); }
  }
  toast(`一键签到：${ok} 家已处理${no ? `，${no} 家没成` : ''}`, no === 0);
  if (bad.length) for (const b of bad.slice(0, 3)) toast(b, false);
  await invoke('action', { payload: { action: 'checkin_refresh' } }).catch(() => null);
  load();
}

/* ---------------- 每日签到 ---------------- */
/* agenthub 那套 store 名与驾驶舱的程序 id 不完全一样（Trae 两边就不同名），
   名字在这张表里给全，别去 S.data.apps 里猜——猜错的表现是这一行显示成空白产品名 */
const CI_NAMES = {
  workbuddy: 'WorkBuddy', 'workbuddy-intl': 'WorkBuddy 国际版',
  'workbuddy-activity': 'WorkBuddy 活跃地图', 'workbuddy-travel': 'WorkBuddy 旅行猫',
  'trae-cn': 'Trae CN', minimax: 'MiniMax Code', astudio: 'AStudio',
  'qoder-cn': 'Qoder CN', qoder: 'Qoder',
};
let ciLoading = false;

/* 进页面 / 点刷新 / 领完才真去问一次，4 秒轮询绝不重打。
   `checkin_view` 现在**立刻返回**（缓存，或上次落盘的那一份）+ 后台去刷，所以这里
   不会再有"进一次等半天"；真正刷出来的那份由 4 秒一轮的 state 带回来
   （见 render() 里同步 S.checkin 的那两行）。「刷新状态」按钮走 checkin_refresh，
   那一下是同步的——她明确点了要等结果，那就等。 */
async function loadCheckin(force) {
  if (ciLoading) return;
  const cur = S.checkin;
  if (!force && cur && cur.ok && (Date.now() / 1000 - (cur.fetched_at || 0)) < 120) return;
  ciLoading = true;
  if (force) toast('正在问各家今天的状态…', true);
  const r = await invoke('action', { payload: { action: force ? 'checkin_refresh' : 'checkin_view' } })
    .catch((e) => ({ ok: false, msg: String(e) }));
  ciLoading = false;
  if (r && r.ok) {
    // 别用旧的把新的盖回去：后台刷出来的那份可能比这次返回的还新
    if ((r.fetched_at || 0) >= ((S.checkin || {}).fetched_at || 0)) S.checkin = r;
  } else if (!cur) {
    S.checkin = r || { ok: false, msg: '没读到' };
  }
  if (force) toast(r && r.ok ? '状态已更新' : `读不到：${(r && r.msg) || '未知'}`, !!(r && r.ok));
  renderCheckin();
}

/* 状态列说什么：词和样式分开给，首页概览要的是同一句话。
   `pending` 是"这次还没读到"——列表已经画出来了，状态正在补，不该写成"状态未知"。 */
function ciState(it) {
  const w = it.words || {};
  if (it.pending) return { cls: 'pending', word: '查询中…' };
  if (it.error) return { cls: 'bad', word: '读不到' };
  // 旅行猫是状态机不是签到：在路上要说"猫猫在路上"，写成"状态未知"等于什么都没说
  if (it.mode === 'travel' && it.next_action === 'wait') return { cls: 'wait', word: w.wait_badge || '猫猫在路上' };
  if (it.signed_today === true) return { cls: 'done', word: w.done || '今天已签' };
  if (it.claimable === true) return { cls: 'go', word: w.go || '今天可领' };
  // 接口两条都正常返回、只是今天还没到账：那是"还没登录过"，不许画成红色"读不到"
  if (it.need_login) return { cls: 'wait', word: w.wait || '今天还没登录' };
  if (it.activity_on === false) return { cls: 'off', word: '活动未开' };
  return { cls: 'off', word: '状态未知' };
}

/* 操作列该写哪个词——**这一列讲的是"现在这一步要做什么"**，不是状态复读：
   已领完 ≠ 不用领 ≠ 可领；旅行猫还有 派出 / 等待 / 领取 三个真动作。
   （她 2026-10-15 圈出来："不用领和已领完完全是两个概念"） */
function ciActionWord(it) {
  const w = it.words || {};
  if (it.error) return '';
  if (it.mode === 'travel') {
    if (it.next_action === 'depart') return w.depart || '派出';
    if (it.next_action === 'wait') return w.wait || '等待中';
    if (it.next_action === 'claim') return w.claim || '领取';
    return w.done_act || '今天已领完';
  }
  if (it.signed_today === true) return w.done_act || '今天已领完';
  if (it.claimable === true) return w.act || (it.mode === 'activity' ? '发一句对话' : '立刻签到');
  return '今天不用领';
}

function ciBadge(it) {
  const s = ciState(it);
  return el('span', { class: `ci-badge ${s.cls}` }, s.word);
}

/* 一行一个按钮，写的是"这一步到底要做什么"。写操作（领取 / 发对话 / 派出）都要先确认。 */
function ciAction(it, name) {
  if (it.pending) {
    return btn('查询中…', 'sm', () => {}, '状态正在读，马上就有');
  }
  if (it.need_login) {
    // 这一家今天还没登录过，光把按钮摆成"不用领"等于什么都没做——直接帮她把客户端拉起来
    return btn('打开客户端登一次', 'sm primary', async () => {
      const r = await invoke('action', { payload: { action: 'launch_app', app: it.product, name: '' } });
      toast(r && r.ok ? '已打开客户端，登进去之后回这页点一次刷新' : (r && r.msg) || '打开失败', !!(r && r.ok));
      if (r && r.ok) setTimeout(() => loadCheckin(true), 25000);
    }, '每日积分要今天登录过一次才发；点这个把客户端打开，登录后我自动回来补状态');
  }
  const w = it.words || {};
  const travel = it.mode === 'travel';
  const act = it.mode === 'activity';      // 国际版那种"当天用过就送"的基础权益，不是签到
  // 旅行猫三个真动作；到了才能领、领过就不能再派
  const step = travel ? (it.next_action || 'wait') : '';
  const doable = travel
    ? (step === 'depart' || step === 'claim')
    : (it.claimable === true);
  const label = ciActionWord(it);
  const title = !doable
    ? (travel
      ? (step === 'wait'
        ? `猫猫还在路上${it.arrive_at ? `，约 ${it.arrive_at} 到` : ''}，到点了这一格会变成「领取」`
        : '今天这一趟已经领过了，明天再来')
      : (it.signed_today === true ? '今天这份已经到手了，明天再来' : '现在没有可领的，点了也是白点'))
    : (travel
      ? (step === 'depart' ? '派猫猫出门旅行（写操作）：每天只能派一次，1~4 小时后到，到点了再回来点「领取」'
        : '领取这一趟的旅行奖励（写操作）')
      : (act ? '发一句对话（写操作）' : '发一次领取请求（写操作）'));
  const b = btn(label, doable ? 'sm primary' : 'sm', async () => {
    if (!doable) return;
    const yes = await askConfirm(
      travel
        ? (step === 'depart'
          ? `要派「${name}」的猫猫出门旅行吗？\n\n每天只能派一次，1~4 小时后到；到点了这一格会变成「领取」。`
          : `要领取「${name}」这一趟的旅行奖励吗？\n\n${it.reward != null ? `${it.reward} 积分，` : ''}领完今天就不能再派了。`)
        : act
          ? `要给「${name}」发一句对话，让服务端认定今天用过吗？\n\n这一档不是签到，是当天用过就送的基础权益，会真起一次命令行会话。`
          : `要给「${name}」领今天的签到积分吗？\n\n领完这一家的当日名额就用掉了，撤回不了。`,
      { title: travel ? (step === 'depart' ? '确认派出' : '确认领取') : (act ? '确认发对话' : '确认领取') });
    if (!yes) return;
    // 账号级那行要把账号名一起发过去：两个号各有各的当日名额，
    // 不带账号就会全落到"当前登着的那个号"头上（2026-10-16 加）
    const r = await invoke('action', {
      payload: { action: 'checkin_claim', app: '', store: it.product, account: it.account || '' },
    });
    const x = (r && r.result) || {};
    // claimed 只在 agenthub 回读状态确认过之后才为真——厂商回一句 200 不算领到。
    // settling 是"写操作做完了、服务端还没结算"，那是等，不是失败。
    toast(x.claimed ? `已领到 ${x.credit != null ? x.credit : ''} ${x.message || ''}`.trim()
      : x.settling ? (x.message || '已经发出去了，等它到账')
        : `没领成：${x.message || x.error || (r && r.msg) || '未知原因'}`,
      !!(x.claimed || x.settling));
    loadCheckin(true);
  }, title);
  if (!doable) b.disabled = true;
  return b;
}

/* 卡片上的签到角标。状态是只读的、信息量小，放得下；动作（去领）留在「每日签到」页，
   点角标直接跳那一页。缓存里没这家（不支持签到）就什么都不画——摆个"未查"是噪音 */
function checkinMark(i) {
  const c = i.checkin;
  if (!c || !c.state) return null;
  const txt = { done: '已签', claimable: '可领', off: '签到未开', unknown: '状态未知', error: '读不到' }[c.state];
  if (!txt) return null;
  const tip = `${c.name || '每日签到'} · ${txt}`
    + (c.streak != null ? ` · 连签 ${c.streak} 天` : '')
    + (c.reward != null ? ` · 每日 ${c.reward}` : '')
    + (c.msg ? `\n${c.msg}` : '\n点一下去看状态或直接领');
  const b = el('span', { class: `cimark ${c.state}`, title: tip }, txt);
  b.addEventListener('click', (e) => { e.stopPropagation(); S.view = 'checkin'; render(); });
  return b;
}

/* 提醒是只读的，跟"能不能自动领"两回事：领取要验证码的那几家，照样该到点提醒我去点 */
async function saveCheckinMute(store, muted) {
  const mute = { ...(S.checkinMute || {}) };
  if (muted) mute[store] = true; else delete mute[store];
  S.checkinMute = mute;
  await run('checkin_auto', { remind_mute: mute });
}

/* 一行九格，全部塞进同一个 grid 容器（父级 display:grid、这里 display:contents），
   列宽只在 .citab 里定义一次，所以"每日多少积分""今天不用领"这些同类内容必然落在同一列。
   以前每行自己 flex 排，一家少个字段整行就错位——你圈出来的就是这个。 */
const CI_COLS = ['产品', '状态', '连签', '每日', '累计', '活动截止', '操作', '自动领', '提醒'];

function ciRule(grid) { grid.appendChild(el('div', { class: 'ci-rule' })); }

function ciCells(grid, it, autoOn, opts) {
  const o = opts || {};
  const name = it.name || CI_NAMES[it.product] || it.product;
  // 产品行的标题格：有账号子行时左边挂一个折叠箭头（▾ 展开 / ▸ 收起）。
  // 账号行缩进一格，左边画一条竖线，一眼看出它属于上面那家。
  const nameCell = el('div', { class: `ci-name${o.account ? ' acc' : ''}` });
  // 箭头和名字必须**同一行**：.ci-name 是 column flex，箭头单独放进去会独占一行
  const nmRow = el('div', { class: 'ci-nmrow' });
  if (o.kids) {
    const f = el('button', {
      class: 'ci-fold', type: 'button',
      title: o.folded ? `展开 ${o.kids} 个账号` : `收起 ${o.kids} 个账号`,
    }, o.folded ? '▸' : '▾');
    f.addEventListener('click', () => {
      S.ciFold = S.ciFold || {};
      S.ciFold[it.product] = !o.folded;
      renderCheckin();
    });
    nmRow.appendChild(f);
  }
  nmRow.appendChild(el('span', { class: 'ci-nm' }, name));
  if (o.kids) nmRow.appendChild(el('span', { class: 'ci-kids' }, `${o.kids} 个账号`));
  nameCell.appendChild(nmRow);
  if (it.note) nameCell.appendChild(el('span', { class: 'ci-note' }, it.note));
  grid.appendChild(nameCell);
  // 有账号子行时，产品行的状态列换成**汇总**：「1/2 已签」。
  // 不然那一格写的是"这台机器现在登着的那个号"的状态，跟底下两行并排看着就像自相矛盾。
  if (o.agg) {
    const all = o.agg.total > 0 && o.agg.done === o.agg.total;
    grid.appendChild(el('span', {
      class: `ci-badge ${all ? 'done' : 'go'}`,
      title: `名下 ${o.agg.total} 个账号里今天已有 ${o.agg.done} 个领到；各自的详细状态看下面那几行`,
    }, `${o.agg.done}/${o.agg.total} 已签`));
  } else {
    grid.appendChild(ciBadge(it));
  }
  if (it.error) {
    // .ci-err 自己 grid-column: 3 / -1 已经占满剩下的列，再补六个空格会把它们
    // 挤进下一条隐式行——那也是一段对不上的线
    grid.appendChild(el('div', { class: 'ci-err', title: it.error }, it.error));
    return;
  }
  const u = it.unit || '积分';
  grid.appendChild(el('span', { class: 'ci-num' }, it.streak != null ? `${it.streak} 天` : '官方没给'));
  grid.appendChild(el('span', { class: 'ci-num' }, it.reward != null ? `${it.reward} ${u}` : '—'));
  grid.appendChild(el('span', { class: 'ci-num', title: it.days_signed ? `本季签了 ${it.days_signed} 天` : '' },
    it.total != null ? `${it.total}` : '—'));
  grid.appendChild(el('span', { class: 'ci-num' }, it.ends_on || '—'));
  grid.appendChild(el('div', { class: 'ci-act' }, ciAction(it, name)));
  // 「自动领」「提醒」是**按产品**设的开关，账号行不重复摆——同一个产品摆两排同样的勾，
  // 勾哪一个生效就说不清了。这里写清楚它跟着上面那行走。
  if (o.account) {
    const tip = '自动领 / 提醒是**按产品**设的开关，在上面那一行（' + (CI_NAMES[it.product] || it.product) + '）上勾';
    grid.appendChild(el('span', { class: 'cick no', title: tip }, '同上'));
    grid.appendChild(el('span', { class: 'cick no', title: tip }, '同上'));
    return;
  }
  // 只有厂商给了独立状态接口、领取又不需要验证码的，才允许交给程序自动跑
  if (it.pending) {
    // 状态还没读到，这一格先空着——写「不可」是在替一家还没问过的产品下结论
    grid.appendChild(el('span', { class: 'cick no', title: '状态还没读到，等一下就知道了' }, '—'));
  } else if (it.auto === true) {
    const cb = el('input', { type: 'checkbox', class: 'ciauto' });
    cb.checked = !!autoOn;
    cb.title = '到时间窗里让驾驶舱自己领一次（一天最多一次）；不勾就等你自己点';
    cb.addEventListener('change', () => saveCheckinAuto(it.product, cb.checked));
    grid.appendChild(el('label', { class: 'cick' }, cb));
  } else {
    grid.appendChild(el('span', { class: 'cick no', title: '这家没有独立状态接口，或领取要过验证码/要在网页里点，不交给程序自动跑' }, '不可'));
  }
  const rb = el('input', { type: 'checkbox', class: 'ciwarn' });
  rb.checked = !(S.checkinMute || {})[it.product];
  rb.title = '到点提醒我今天去领（只读提醒，不会替我领）';
  rb.addEventListener('change', () => saveCheckinMute(it.product, !rb.checked));
  grid.appendChild(el('label', { class: 'cick' }, rb));
}

async function saveCheckinAuto(store, on) {
  const auto = {};
  for (const it of (S.checkin && S.checkin.items) || []) {
    if (it.auto === true) auto[it.product] = !!(S.checkinAuto || {})[it.product];
  }
  auto[store] = on;
  if (on) auto[store] = true; else delete auto[store];
  S.checkinAuto = auto;
  // 整张 auto 表一次发回去：后端是"给了就整段替换"，一条条发会把别的家顶掉
  const r = await run('checkin_auto', { enabled: !!S.checkinEnabled, auto });
  return r;
}

function renderCheckin() {
  const host = $('checkin-body');
  if (!host) return;
  // 重建会让内容瞬间塌高，外层 pagebody 的 scrollTop 被浏览器夹回 0，先记住再还回去
  const pb = host.closest('.pagebody');
  const keep = pb ? pb.scrollTop : 0;
  clear(host);
  const d = S.checkin;
  const st = (S.data && S.data.settings && S.data.settings.checkin) || {};
  S.checkinAuto = S.checkinAuto || st.auto || {};
  S.checkinMute = S.checkinMute || st.remind_mute || {};
  S.checkinEnabled = st.enabled;
  host.appendChild(el('div', { class: 'hintline' },
    el('span', {}, '查状态是只读的，随时可以刷；'),
    el('b', {}, '领取是写操作'),
    el('span', {}, '——领走的当日名额退不回来，所以默认每家都要你亲手点一下。'
      + '勾了「自动领」的那几家，驾驶舱会在时间窗里挑一天里的一次自己领，同一家一天只跑一次。'
      + '「提醒」默认开着：到点把今天还没领的报一次，一天只报一次，不催第二遍。'
      + '厂商没给状态接口、或领取要过验证码的，这里一律标「不可自动」，但照样会提醒你。')));
  const bar = el('div', { class: 'cibar' },
    el('label', { class: 'cisw' }, (() => {
      const cb = el('input', { type: 'checkbox' });
      cb.checked = !!st.enabled;
      cb.addEventListener('change', async () => {
        S.checkinEnabled = cb.checked;
        await run('checkin_auto', { enabled: cb.checked, auto: S.checkinAuto || {} });
      });
      return cb;
    })(), '允许自动签到'));
  const from = el('input', { type: 'number', min: 0, max: 23, value: st.window_start ?? 6, class: 'cinum' });
  const to = el('input', { type: 'number', min: 1, max: 24, value: st.window_end ?? 12, class: 'cinum' });
  bar.appendChild(el('span', { class: 'cilab' }, '自动领时间窗'));
  bar.appendChild(from);
  bar.appendChild(el('span', { class: 'cilab' }, '点到'));
  bar.appendChild(to);
  // 提醒：默认开。到点把"今天还有几家没领"报一次，一天只报一次
  const warn = el('input', { type: 'checkbox' });
  warn.checked = st.remind !== false;
  warn.addEventListener('change', async () => {
    await run('checkin_auto', { remind: warn.checked });
  });
  bar.appendChild(el('label', { class: 'cisw' }, warn, '到点提醒'));
  const at = el('input', { type: 'text', value: st.remind_at || '10:05', class: 'cinum wide', title: '每天几点提醒（HH:MM，按本机时间）' });
  bar.appendChild(at);
  bar.appendChild(btn('保存', 'sm', async () => {
    await run('checkin_auto', {
      enabled: !!S.checkinEnabled, auto: S.checkinAuto || {},
      window_start: Number(from.value) || 0, window_end: Number(to.value) || 24,
      remind_at: /^\d{1,2}:\d{2}$/.test(at.value.trim()) ? at.value.trim() : '10:05',
    });
  }));
  bar.appendChild(el('span', { class: 'cilab' },
    d && d.placeholder ? '正在问各家…'
      : d && d.fetched_at
        ? `状态查于 ${stampTime(d.fetched_at)}${d.refreshing || ciLoading ? ' · 正在刷新…' : ''}`
        : '还没查过'));
  host.appendChild(bar);
  if (!d || !d.ok) {
    host.appendChild(el('div', { class: 'qempty' }, `读不到签到状态：${(d && d.msg) || '没数据'}`));
    return;
  }
  const items = d.items || [];
  // 两级：产品行 + 它名下的账号行（`scope:"account"`，`parent` 指着产品）。
  // 账号行只有"这台机器上每个号都留着自己那份登录态"的产品才有（现在 Qoder 系与 WorkBuddy 系）——
  // 其余几家整机一份登录态，硬拆只会给出串号的假答案，所以只有产品行。
  const prods = items.filter((x) => x.scope !== 'account');
  const today = (items[0] && items[0].today) || '';
  const go = prods.filter((x) => x.claimable === true && !x.error);
  const nAcc = items.length - prods.length;
  $('checkin-count').textContent = d.placeholder
    ? '正在读各家状态…'
    : `${prods.filter((x) => x.signed_today === true).length} 家已签 · ${go.length} 家可领`
      + (nAcc ? ` · 另有 ${nAcc} 个账号单独看着` : '');
  if (!items.length) host.appendChild(el('div', { class: 'qempty' }, '一个都没查到'));
  // 表头 + 所有行共用这一个 grid：列宽定义一次，同类内容不可能错位。
  // **列表本身是纯 UI，不等任何接口**：一次都没查到时后端会先给一份占位行（状态列写"查询中"），
  // 查过的直接把上次那份摆出来——以前这里要等六家接口全回来才画第一行。
  const grid = el('div', { class: 'citab' });
  for (const c of CI_COLS) grid.appendChild(el('div', { class: `ci-h${c === '产品' ? ' l' : ''}` }, c));
  // 分隔线一格一条，落点就跟着每格自己的高度跑（带 align-self:center 的那几格只有内容高），
  // 一行里会出现两三种不同的 y——就是"线不对齐"。改成一行一条、跨满整行。
  S.ciFold = S.ciFold || {};
  for (const it of prods) {
    const kids = items.filter((x) => x.scope === 'account' && x.parent === it.product);
    const folded = !!S.ciFold[it.product];
    const agg = kids.length
      ? { done: kids.filter((x) => x.signed_today === true).length, total: kids.length }
      : null;
    ciCells(grid, it, !!(S.checkinAuto || {})[it.product], { kids: kids.length, folded, agg });
    ciRule(grid);
    if (folded) continue;
    for (const k of kids) { ciCells(grid, k, false, { account: true }); ciRule(grid); }
  }
  host.appendChild(grid);
  // 还在等它到账的那几家：写明白"在等什么、等了多久"，别让她以为忘了
  const pend = Object.values((d.state && d.state.pending) || {});
  if (pend.length) {
    host.appendChild(el('div', { class: 'cipend' },
      el('b', {}, '正在等到账'),
      ...pend.map((p) => el('span', { class: 'cihit' },
        `${p.label || p.today || ''}：${p.expect || '今天这份积分'}还没结算到账，驾驶舱每几分钟替你看一次，`
        + '到账了会在这里提示你（这是服务端按天批量发放的，发放时刻不固定）'))));
  }
  const log = ((d.state && d.state.log) || []).slice(-6).reverse();
  if (log.length) {
    host.appendChild(el('div', { class: 'cilog' },
      el('b', {}, '最近领过的'),
      ...log.map((x) => el('span', { class: 'cihit' },
        `${x.at || ''} ${CI_NAMES[x.store] || x.store} · `
        + (x.claimed ? `领到 ${x.credit != null ? x.credit : ''}` : x.already ? '已领过' : `没领成：${x.message || x.error || ''}`)))));
  }
  if (today) host.appendChild(el('div', { class: 'cihint' }, `厂商侧的「今天」按本机日期算：${today}`));
  if (pb) pb.scrollTop = keep;
}

/* ---------------- 协作（AgentHub 工作流看板 iframe） ---------------- */
const AGENTHUB_UI = 'http://127.0.0.1:8766/';
let collabState = '';            // '' 未探测 · 'up' 已连接 · 'down' 未启动
let collabTimer = null;

async function probeCollab() {
  try {
    await fetch(AGENTHUB_UI + 'api/overview', { mode: 'no-cors', cache: 'no-store' });
    return true;                 // opaque 响应也算通：只验证端口活着
  } catch { return false; }
}

async function ensureCollab(force) {
  const st = $('collab-status');
  const off = $('collab-off');
  const fr = $('collab-frame');
  const hint = $('collab-hint');
  if (!fr || !off) return;
  if (force) collabState = '';
  if (collabState === '') {
    st.textContent = '探测 AgentHub 看板…';
    collabState = (await probeCollab()) ? 'up' : 'down';
  }
  if (collabState === 'up') {
    if (fr.getAttribute('src') !== AGENTHUB_UI) fr.setAttribute('src', AGENTHUB_UI);
    fr.hidden = false; off.hidden = true; hint.hidden = true;
    st.textContent = '已连接 ' + AGENTHUB_UI.replace(/\/$/, '');
  } else {
    fr.removeAttribute('src');
    fr.hidden = true; off.hidden = false;
    hint.hidden = false;
    hint.textContent = '没有探测到本机 8766 端口的看板服务；启动后点「重新连接」，我会每 5 秒自动再试。';
    st.textContent = '看板未启动';
    if (!collabTimer) {
      collabTimer = setTimeout(() => {
        collabTimer = null;
        if (S.view === 'collab') ensureCollab(true);
      }, 5000);
    }
  }
}


/* ---------------- 账号库 ---------------- */
function renderToolbar(d) {
  const app = d.apps.find((a) => a.id === S.cur);
  $('cur-title').textContent = app ? app.name : '—';
  const list = d.instances.filter((i) => i.app === S.cur);
  $('cur-count').textContent = list.length ? `${list.length} 个账号` : '';
  const cap = $('cur-cap');
  const hint = $('cur-hint');
  clear(cap); clear(hint);
  if (!app) { cap.hidden = true; hint.hidden = true; return; }
  cap.hidden = false;
  cap.className = `pill ${app.isolatable ? 'yes' : 'no'}`;
  cap.append(app.isolatable ? '可多开' : (app.shared_profile ? '账号共用一份档案' : '仅可换号'));
  hint.hidden = false;
  const forget = $('btn-forget');
  if (forget) forget.hidden = !(app.removed);
  if (!app.installed) {
    hint.append(app.removed
      ? '该程序的可执行文件已不存在（可能已卸载）。账号记录仍保留，可点右上「清理记录」移除这条程序。'
      : '未找到该程序的可执行文件，无法启动；若已卸载可从列表忽略它。');
  } else if (app.isolatable) {
    hint.append('这类程序支持独立数据目录：每个账号一份目录，可同时开多个窗口。点「＋ 添加账号」按向导走。');
  } else if (app.shared_profile) {
    hint.append(app.auth_switch
      ? '该程序不能同时多开，几个账号还共用同一份档案。驾驶舱存过哪个号的登录态，就能一键切过去（会重启一次客户端——它只在启动时读登录文件，这次重启省不掉）；'
        + '没存过的号先在程序里登一次，登进来我们自动收下这份登录态，以后来回都是一键。'
      : '该程序会把第二次启动转交给已开窗口，不能同时多开；它名下几个账号又共用同一份档案，'
        + '所以切换账号就在程序里做：退出登录 → 登另一个号。不用关掉程序，驾驶舱不搬文件、也不会结束任何进程，'
        + '登进来之后对应那张卡会自己标成「本机登录中」。');
  } else {
    hint.append(
      '该程序会把第二次启动转交给已开窗口，因此不能同时多开。多账号请用「设为当前」换号：'
      + '把默认档案目录指向某个账号的目录（零额外空间），一次登录一个账号，随时切回。换号要先关掉该程序窗口。');
  }
  // 换号记录指向一个已被删掉的账号：档案目录还挂着 junction，程序会读到一个不存在的档案。
  // 不给个还原入口，用户只能看着侧栏那个失效的 ⇄ 标记发愁（ZCode 就卡在这）
  if (app.swap_orphan) {
    hint.append(el('span', { class: 'orphan' },
      `档案目录还被换号接管着，但账号「${app.swapped}」已经不存在了。`,
      btn('还原默认档案', 'danger sm', () => run('swap_clear', { app: app.id }),
        '删掉那个失效的目录链接，把你原来的档案目录改回原名（零风险，不会动数据）')));
  }
  // 「这台机器现在登的是谁」——产品级的一份真值，必须摆在卡片之外说。
  // 不摆出来，卡片停在旧号就会被当成"刷新没用"，而实际是本机已经换成另一个号了。
  if (app.local_quota) {
    const mv = (app.machine && app.machine.value) || {};
    const ml = el('span', { class: 'orphan' });
    if (mv.ok) {
      // 单位跟着读数走：DSH 是元、Marvis 是 token，接口没给单位就只写数字，不硬套"积分"
      const mu = mv.unit ? ` ${utxt(mv.unit)}` : '';
      ml.append(`本机当前登录：${mv.account}${mv.user_type ? `（${mv.user_type}）` : ''}`
        + ` · 剩 ${nfmt(mv.left)}${mu}`
        + `${mv.total != null ? ` / 共 ${nfmt(mv.total)}${mu}` : '（接口没给总额）'}`
        + ` · 来源 ${mv.source || '?'}${mv.auth_from ? ` · 读的 ${mv.auth_from}` : ''}`);
    } else if (mv.unsupported) {
      ml.append('本机当前登录：这个程序没有本机直读通道，读不到它现在登的是谁');
    } else if (app.machine && app.machine.refreshing) {
      ml.append('正在读本机当前登录的账号…');
    } else {
      ml.append(`本机当前登录：读不到（${mv.err || mv.msg || '未知'}）`);
    }
    ml.append(btn('重读', 'sm', async () => {
      const r = await invoke('action', { payload: { action: 'machine_login', app: app.id, refresh: true } });
      toast(r && r.ok ? `本机现在登的是「${r.account}」` : `读不到：${(r && (r.err || r.msg)) || '未知'}`,
        !!(r && r.ok));
      load();
    }, '解一次本机当前默认档案里的登录态（约 1 秒）'));
    hint.appendChild(ml);
  }
  const nb = $('btn-new');
  clear(nb);
  nb.append('＋ 添加账号');
  nb.title = '为这个程序再加一个账号档案；向导会带你完成建目录、登录、读额度';
}

// 服务端读不成、只剩客户端旧缓存时（典型：Qoder 登录令牌过期，本地只剩 8 月的快照）：
// 把"这不是实时余额"直接写在卡片里，别让用户拿旧数做判断
function staleNote(q) {
  if (!q.stale) return null;
  const expired = /TOKEN_EXPIRE|UNAUTHORIZED|401/i.test(q.live_error || '');
  return el('div', { class: 'stale-note', title: q.live_error || '' },
    el('b', {}, expired ? '⚠ 登录令牌已过期 · 下面是旧缓存' : '⚠ 读不到服务端 · 下面是旧缓存'),
    el('span', {}, expired
      ? '打开一次该产品让它自动续期，或重新登录后再点一次卡片上的「刷新额度」'
      : '这份数是客户端上次联网时留下的，可能已经过期'));
}

/* ================= 额度显示层：档名 / 单位 / scope 一律跟着各产品自己的口径走 =================
   数据层（agenthub + local_quota/state）给什么档名、什么单位、有没有 scope，界面就照那样画：
   不套 WorkBuddy 那三个固定名字，也不把接口没给的数当成 0。 */

/* 单位：接口给的 unit 是厂商原话（credits / 积分 / token / 元 / 次…）。
   只把英文 credits 归一成中文「积分」，其余照原样显示——
   写死成「积分」会把 DSH 的 6.24 元和 Marvis 的 98,557,013 token 说成假话。 */
const UNIT_CN = { credits: '积分', credit: '积分', point: '积分', points: '积分' };
function utxt(u) {
  const k = String(u == null ? '' : u).trim();
  if (!k) return '积分';
  return UNIT_CN[k.toLowerCase()] || k;
}

/* 接口没给的数就是 null，不许在这一步悄悄变成 0 */
function numv(v) {
  if (v === null || v === undefined || v === '') return null;
  const n = Number(v);
  return Number.isFinite(n) ? n : null;
}

/* 一个产品底下几档的单位：一致就统一显示；
   不一致时大字行按第一档的单位标，小字里把出现的几种单位都说出来，不假装它们是同一种数 */
function unitPlan(groups) {
  const seen = [];
  for (const g of groups) {
    const u = utxt(g.unit);
    if (!seen.includes(u)) seen.push(u);
  }
  return { main: seen[0] || '积分', all: seen, mixed: seen.length > 1 };
}

/* scope（这一档能花在什么上）→ 档名后面那枚小徽章。
   只有"这一档只能花在特定模型上"才值得钉徽章——用户点名要把讯飞星火那档标出来。
   其余 scope 是说明句（"按 token 计数"、"不到期"、"长期有效钱包"），钉成「专属」
   是错的；完整句子在明细面板里整行写出来，这里只留 title 悬浮。
   scope 普遍是一长句，从中间截半句会读成病句，所以短到能独立成词才用原文。 */
const SCOPE_EXCLUSIVE = /(仅|只)[^，,。；;]{0,8}用|专属|不含|限[^，,。；;]{0,8}模型/;
function scopeTag(g) {
  const s = String((g && g.scope) || '').trim();
  if (!s || !SCOPE_EXCLUSIVE.test(s)) return null;
  const short = s.length <= 8 && !/[，,。.；;：:、（）()【】\s]/.test(s);
  return el('span', { class: 'scopeb', title: `这一档只用于：${s}` }, short ? s : '专属');
}

/* 一档里 total、left、逐笔全是空或 0：这一行没话可说，不占地方 */
function quotaHasData(g) {
  return !!((g.total != null && g.total !== 0) || (g.left != null && g.left !== 0)
    || (g.items || []).length || (g.live_count || 0) || (g.expired_count || 0));
}
/* 渲染层统一的取数入口：归一化 + 丢掉空档 */
function quotaRows(i) {
  return quotaGroups(i).filter(quotaHasData);
}

/* 一档都没有（只有登录账号、接口读不到额度）：老实说读不到，并把后端那句原因带上，不许显示 0 */
function quotaMiss(q) {
  const reason = String(q.stale_hint || q.live_error || '').trim();
  // 还没读过（fetched_at 都没有）不能说成"厂商没这一项"——那是没读，不是读不到
  if (!q.fetched_at && !q.stale && !reason) {
    return '还没读到额度　点卡片上的「↻ 刷新额度」，或「账号设置 → 额度与用量 → 贴官网积分页截图识别」';
  }
  const head = q.stale
    ? '这个产品的额度读不到：服务端没读成，缓存里也没有可用额度记录'
    : '这个产品的额度读不到——厂商接口里没有这一项，或本机还没有可读的登录态';
  const act = q.stale
    ? '让该产品重新登录一次，再点一次卡片上的「刷新额度」'
    : '能本机直读或能登官网的点「刷新额度」，两条路都读不到的贴官网积分页截图识别';
  return [head, reason ? `原因：${reason}` : '', act].filter(Boolean).join('　');
}

/* 积分归一化：agenthub 给的分组优先；老数据或没有分组的产品，把逐桶当成一组。
   界面只认这一种形状，不再各处自己拼。
   null 的语义要保住：接口没给 total / used / items[].left 时是 null，不是 0。
   这里一个 null 都不许落成 0，也不许在 left 缺失时拿 total-left 反推 used——
   AStudio「国庆每日登录星火积分赠送」那种「获得明细」表本来就没有逐笔剩余列，
   反推出来的 5,000 会被画成「已使用 5,000 / 5,000」，等于宣称这一笔已经用完（假话）。 */
function quotaGroups(i) {
  const q = i.quota || {};
  const num = numv;
  const diff = (total, left) => (total != null && left != null ? Math.max(0, total - left) : null);
  const norm = (g) => {
    const items = (g.items || []).map((it) => {
      const left = num(it.left), total = num(it.total), used = num(it.used);
      return { ...it, left, total, used: used != null ? used : diff(total, left) };
    });
    const left = num(g.left), total = num(g.total), used = num(g.used);
    return {
      ...g, items, left, total, unit: g.unit || 'credits', scope: String(g.scope || '').trim(),
      used: used != null ? used : diff(total, left),
    };
  };
  if (Array.isArray(q.groups) && q.groups.length) return q.groups.map(norm);
  const cr = Array.isArray(q.credits) ? q.credits : [];
  if (!cr.length) {
    // 只有一个笼统的剩余/总额（接口抓取或老数据）：也当一档画，否则卡片会显示"没读到额度"。
    // "未给出"/"无限期" 不算有到期信息——Xiaomi MiMo 只读得到登录账号，
    // 三个数全空时合成一档会画出「总剩余 —」+ 一句"官方没给总额，只有剩余"的病句。
    const left = num(q.left), total = num(q.total);
    const hasExp = q.expires_on && q.expires_on !== EXP_NONE && q.expires_on !== '无限期';
    if (left == null && total == null && !hasExp) return [];
    return [norm({
      name: '积分', unit: q.unit || 'credits', left, total, used: diff(total, left),
      live_count: 1, expired_count: 0, expires: hasExp ? q.expires_on : EXP_NONE,
      days_left: q.days_left, items: [],
    })];
  }
  // 逐桶有 category 就按 category 分，没有就一组"积分"
  const by = new Map();
  for (const c of cr) {
    const name = c.category || '积分';
    if (!by.has(name)) by.set(name, { name, unit: c.unit || 'credits', items: [] });
    by.get(name).items.push({
      label: c.label || '未命名', pool: c.label || '', detail: c.note || '',
      left: c.left, total: c.total,
      used: c.used != null ? c.used : (c.total != null && c.left != null ? Math.max(0, c.total - c.left) : null),
      unit: c.unit || 'credits', expires_on: c.expires_on || '', days_left: c.days_left,
      expired: c.days_left != null && c.days_left < 0,
    });
  }
  return [...by.values()].map((g) => {
    // 逐笔里缺的数就让它缺：只有给了数的笔参与合计，一笔都没给就整档 null（不许当 0）
    const sum = (k) => {
      const vals = g.items.map((it) => num(it[k])).filter((v) => v != null);
      return vals.length ? vals.reduce((s, v) => s + v, 0) : null;
    };
    const live = g.items.filter((it) => !it.expired);
    const exp = live.map((it) => it.expires_on).filter((x) => x && x !== EXP_NONE && x !== '无限期').sort()[0] || '';
    return norm({
      ...g, left: sum('left'), total: sum('total'), used: sum('used'),
      live_count: live.length, expired_count: g.items.length - live.length,
      live_left: sum('left'),
      expires: exp, days_left: live.map((it) => it.days_left).find((x) => x != null) ?? null,
    });
  });
}

/* 百分比只在真的给了分母时才算：total 为 null / 0 一律返回 null，
   调用方据此同时关掉进度条和"已用 X / Y"副行 */
function quotaPct(total, left) {
  if (!total || left == null) return null;
  return Math.max(0, Math.min(100, (left / total) * 100));
}

/* 百分比上界面：29,971 / 30,000 四舍五入是 100%，可它明明用掉了 29——
   夹到 1~99，只有真的一分没动 / 一点不剩才写 0% / 100% */
function pctTxt(gp) {
  if (gp == null) return '';
  if (gp >= 100) return '100%';
  if (gp <= 0) return '0%';
  return `${Math.min(99, Math.max(1, Math.round(gp)))}%`;
}

/* 汇总口径：单位不一致时大字只加第一档那种单位的档——把 6.24 元和 1,500 积分加成
   "总剩余 1,506.24 元" 是小字注明也救不回来的假账。 */
function quotaAgg(groups) {
  const up = unitPlan(groups);
  const same = up.mixed ? groups.filter((g) => utxt(g.unit) === up.main) : groups;
  const other = up.mixed ? groups.filter((g) => utxt(g.unit) !== up.main) : [];
  const kl = same.filter((g) => g.left != null);
  const kt = same.filter((g) => g.total != null);
  return {
    up, same, other,
    left: kl.length ? kl.reduce((s, g) => s + g.left, 0) : null,
    total: kt.length ? kt.reduce((s, g) => s + g.total, 0) : null,
    missLeft: same.length - kl.length,
    missTotal: same.length - kt.length,
  };
}

/* 缺数说明那一小句：谁没给总额、谁没给剩余、单位是不是好几种 */
function quotaCaveat(a, groups) {
  const n = [];
  const one = groups.length === 1;
  if (a.missTotal && a.missLeft && a.missTotal === a.missLeft) {
    // 两个数都没给：分开念会变成"官方没给总额，只有剩余　官方没给剩余"这种自相矛盾的病句
    n.push(one ? '厂商没给积分数' : `${a.missTotal} 档厂商没给积分数`);
  } else {
    if (a.missTotal) n.push(one ? '官方没给总额，只有剩余' : `${a.missTotal} 档官方没给总额，比例先不算`);
    if (a.missLeft) n.push(one ? '官方没给剩余' : `${a.missLeft} 档官方没给剩余`);
  }
  if (a.up.mixed) {
    n.push(`单位有 ${a.up.all.join(' / ')} 两种，大字只加 ${a.up.main} 那一类`
      + `（另 ${a.other.length} 档不计入）`);
  }
  return n.join('　');
}

/* 排序用的到期键：各种写法都收敛成 dtxt 的 "YYYY-MM-DD HH:MM"（可直接字典序比较），
   没给日期和"无限期"归到最末，别让它们顶到"最早作废"的位置上 */
function expKey(v) {
  const s = String(v == null ? '' : v).trim();
  if (!s || s === EXP_NONE || s === '无限期' || s === '—') return '9999-12-31 23:59';
  return dtxt(s);
}

function expCls(days, stale) {
  if (stale || days == null) return 'exp';
  if (days < 0 || days <= 7) return 'exp danger';
  if (days <= 30) return 'exp warn';
  return 'exp';
}
function expText(days) {
  if (days == null) return '';
  return days < 0 ? '已过期' : days === 0 ? '今天到期' : `${days} 天后过期`;
}

/* 读数来源的话术。三个来源能给出的东西完全不一样：官网逐包才有每个资源包各自的
   到期日，服务端/本机只有「主额度 + 附加包」两个汇总数。
   这一句是用户判断"明细为什么只剩一笔"的唯一线索，不许省。 */
function srcTxt(s) {
  return { web: '官网逐包', server: '服务端实时', 'local-cache': '客户端缓存' }[s] || '';
}

/* 明细那一份读数的来源与截至时间。汇总数每半小时被本机读数刷一次，而逐笔明细
   只有官网给得回来、粗读数不许覆盖它（local_quota 的守卫），所以两者可能差好几个小时，
   必须单独说出来，不然用户看到的就是"数字对不上"或"明细蜷成一团"。 */
function detailStamp(q) {
  const s = srcTxt(q.detail_source);
  const at = q.detail_at ? stampTime(q.detail_at) : '';
  if (!s && !at) return '';
  return `逐笔明细：${s || '来源未记'}${at ? ` · 截至 ${at}` : ''}`
    + (q.detail_stale ? '（比上面的汇总数旧，点卡片上的「刷新额度」补最新的）' : '');
}

// 官网那次读数没成：逐包额度和各自的到期时间只有官网给，卡上必须留一句
// "为什么没补上、下一步点哪儿"——后端只在真读成时才清空它，所以这句会一直跟着这张卡，
// 直到她把明细补回来。不写这句，卡片就只剩"明细是某天读的"，看起来像读数坏了。
function webNote(q) {
  const e = String(q.web_error || '').trim();
  if (!e || q.stale) return null; // 已经整块标成旧缓存了，不再叠第二句
  return el('div', { class: 'stale-note', title: e },
    el('b', {}, '⚠ 官网逐包明细没读成'),
    el('span', {}, `${e}　点「↻ 刷新额度」会把这个号自己的官网登录窗口打开。`));
}

function quotaBox(i) {
  const q = i.quota;
  const box = el('div', { class: `quota${q.stale ? ' stale' : ''}` });
  const note = staleNote(q);
  if (note) box.appendChild(note);
  const wn = webNote(q);
  if (wn) box.appendChild(wn);
  const groups = quotaRows(i);
  if (!groups.length) {
    if (q.local_used) {
      // 只读得到"累计已用"的产品：不冒充余额
      // 数字是按这个账号自己的档案目录读的，不是全机共享
      box.appendChild(el('div', { class: 'row' },
        el('span', { class: 'cap' }, '该档案累计已用'),
        el('span', { class: 'val' }, nfmt(q.local_used)),
        el('span', { class: 'unit' }, utxt(q.unit || 'credits'))));
      box.appendChild(el('div', { class: 'stamp' }, '剩余额度要官网接口，本地读不到'));
      if (q.fetched_at) box.appendChild(el('div', { class: 'stamp' }, `上次读取 ${stampTime(q.fetched_at)}`));
      return box;
    }
    // 一档都没有（MiMo 那种只有登录账号的）：说"读不到"，不许摆一个 0 出来
    box.appendChild(el('div', { class: 'none', title: String(q.live_error || '').trim() }, quotaMiss(q)));
    return box;
  }
  // 汇总口径：只加给了数的档；单位不一致时只加第一档那种单位的档（小字里说清楚）
  const a = quotaAgg(groups);
  const up = a.up;
  // 后端那份顶层剩余是它自己算好的口径，单位一致时才优先照它
  const left = !up.mixed && numv(q.left) != null ? numv(q.left) : a.left;
  // 明细比汇总数旧时，"共多少"跟最新的汇总数走：拿旧明细的分组求和去配新的剩余，
  // 中间隔几个小时就会算出对不上的"已用"，那是假账
  const aTotal = q.detail_stale && numv(q.total) != null ? numv(q.total) : a.total;
  const used = left != null && aTotal != null ? Math.max(0, aTotal - left) : null;
  const pct = quotaPct(aTotal, left);
  const dl = groups.map((g) => g.days_left).find((x) => x != null);
  box.appendChild(el('div', { class: 'row' },
    el('span', { class: 'cap' }, '总剩余'),
    el('span', { class: 'val' }, nfmt(left)),
    el('span', { class: 'unit' }, up.main),
    q.days_left != null || dl != null
      ? el('span', { class: expCls(q.stale ? null : (q.days_left != null ? q.days_left : dl), q.stale) },
        expText(q.days_left != null ? q.days_left : dl)) : null));
  const srow = [
    pct != null && used != null
      ? el('span', {}, `共 ${nfmt(aTotal)} ${up.main}`) : null,
    pct != null && used != null
      ? el('span', { class: 'used' },
        `已用 ${nfmt(used)}（${pctTxt((used / aTotal) * 100)}）`) : null,
    // 缺数说明（谁没给总额 / 谁没给剩余 / 单位是不是好几种）由 quotaCaveat 一处说完，不重复
    el('span', { class: 'unitnote' }, quotaCaveat(a, groups)),
  ].filter((x) => x && String(x.textContent || '').trim());
  if (srow.length) box.appendChild(el('div', { class: 'row sub' }, srow));
  if (pct != null) box.appendChild(el('div', { class: `bar${pct < 15 ? ' low' : ''}` },
    el('i', { style: `width:${pct}%` })));
  // 每一档一行：厂商自己的档名 + scope 徽章 + 剩/总 + 到期。
  // 只有一个笼统"积分"档、又没有逐笔可看时不重复列——那一行和上面的大字是同一份数
  const list = el('div', { class: 'credits' });
  const showList = groups.filter((g) => !(groups.length === 1 && !g.items.length && !g.scope
    && /^(积分|额度)$/.test(g.name)));
  for (const g of showList) {
    // 官方没给总额时不许拿它算比例：比例落成 0 会画成"剩 128,456（0%）"+ 一条空进度条，
    // 看着像额度用光了，实际只是这一档压根没有分母
    const gp = quotaPct(g.total, g.left);
    const gu = utxt(g.unit);
    const sub = [
      gp != null && g.used != null
        ? el('span', { class: 'cval' }, `已用 ${nfmt(g.used)} / ${nfmt(g.total)} ${gu}`) : null,
      g.live_count > 1 ? el('span', { class: 'cval' }, `${g.live_count} 笔在效期`) : null,
      (g.items || []).length && g.items.some((it) => it.left == null)
        ? el('span', { class: 'cval faint' }, '逐笔只给了获得数') : null,
      gp == null && g.left == null ? el('span', { class: 'cval faint' }, '这一档没给出数值') : null,
    ].filter(Boolean);
    list.appendChild(el('div', { class: 'credit' },
      el('div', { class: 'row' },
        el('span', { class: 'clab', title: g.name }, g.name),
        scopeTag(g),
        el('span', { class: 'cleft' }, g.left == null
          ? '剩余未给出'
          : `剩 ${nfmt(g.left)}${up.mixed ? ` ${gu}` : ''}${gp != null ? `（${pctTxt(gp)}）` : ''}`),
        g.days_left != null || g.expires
          ? el('span', {
            class: expCls(g.days_left, q.stale),
            title: `${g.expires === EXP_NONE || g.expires === '无限期' ? '到期时间' : expiryLabel(g)} ${dtxt(g.expires)}${g.expired_count ? ` · 另有 ${g.expired_count} 笔已过期` : ''}`,
          }, g.expires === EXP_NONE || g.expires === '无限期'
            ? g.expires : expText(g.days_left) || dtxt(g.expires))
          : null),
      sub.length ? el('div', { class: 'row sub' }, sub) : null,
      gp != null ? el('div', { class: 'cbar', title: `剩 ${nfmt(g.left)} / 共 ${nfmt(g.total)} ${gu}` },
        el('b', { style: `width:${gp}%` })) : null));
  }
  if (showList.length) box.appendChild(list);
  if (q.local_used) {
    box.appendChild(el('div', { class: 'stamp' },
      `该档案累计已用 ${nfmt(q.local_used)} ${utxt(groups[0].unit)}`));
  }
  // 数据来源：服务端实时 / 官网逐包 / 客户端旧缓存。用户得能一眼分清这三者
  // （旧缓存已在卡片顶部那条提示里说明，这里就不再重复，只给"实时"一个正面确认）
  const src = q.stale ? '' : (srcTxt(q.source) ? `数据来源 ${srcTxt(q.source)}` : '');
  const meta = [q.fetched_at ? `上次读取 ${stampTime(q.fetched_at)}` : '', src].filter(Boolean).join(' · ');
  if (meta) box.appendChild(el('div', { class: 'stamp' }, meta));
  // 明细比汇总数旧（本机读数只有汇总数、盖不动官网那份逐包明细）：单独一行说清它是几点、哪来的
  if (q.detail_stale) {
    const d = detailStamp(q);
    box.appendChild(el('div', { class: 'stamp warn' }, d || '逐笔明细比上面的汇总数旧'));
  }
  // 登录态是从哪个档案读的：Qoder 0.4.x 换壳后，读到旧档案就会是一份几个月前的旧账，
  // 把路径露出来，出问题时一眼能看出是"读错地方"还是"真没额度"
  if (q.auth_from) {
    const short = String(q.auth_from).split(/[\\/]/).slice(-3).join('\\');
    box.appendChild(el('div', { class: 'stamp ell', title: q.auth_from }, `登录档案 …\\${short}`));
  }
  // 账号名可能很长（邮箱带用户名），单独一行并截断，别把上一行挤爆
  if (q.local_account) {
    box.appendChild(el('div', { class: 'stamp ell', title: q.local_account }, `本机账号 ${q.local_account}`));
  }
  return box;
}

function stampTime(sec) {
  const d = new Date(Number(sec) * 1000);
  const z = (v) => String(v).padStart(2, '0');
  return `${d.getFullYear()}-${z(d.getMonth() + 1)}-${z(d.getDate())} ${z(d.getHours())}:${z(d.getMinutes())}`;
}

/* 按天消耗的两张图：波形图（近期走势）+ 热力图（近一年分布）。
   数据源只有 WorkBuddy 系给得出——它的本地库逐会话记了各资源包扣掉多少 credits。
   别的产品官方只给余额和逐笔到账时刻，没有逐笔消耗时刻，所以读不到就整块不出现：
   摆一张空图占位等于骗人（同"官方没给总额不许当 0"那是一条判据）。 */
const TREND_RANGES = [30, 90, 365];

function svgNode(tag, attrs) {
  const n = document.createElementNS('http://www.w3.org/2000/svg', tag);
  for (const k in attrs) n.setAttribute(k, attrs[k]);
  return n;
}

/* 稀疏的"有数的那几天"要补成连续日序再画：热力图按列=周对齐，
   波形图若不补零，三天没开机会被画成"消耗骤降"，那是把缺数据说成省着用。 */
function trendSeries(t) {
  if (!t || !Array.isArray(t.days) || !t.days.length) return null;
  const sum = new Map();
  for (const d of t.days) {
    const k = String(d.date || '').slice(0, 10);
    if (!k) continue;
    const cur = sum.get(k) || { v: 0, n: 0 };
    cur.v += numv(d.credits) || 0;
    cur.n += numv(d.sessions) || 0;
    sum.set(k, cur);
  }
  const pad = (x) => String(x).padStart(2, '0');
  const today = new Date();
  today.setHours(0, 0, 0, 0);
  const start = new Date(today);
  start.setDate(start.getDate() - 364);
  start.setDate(start.getDate() - start.getDay());
  const cells = [];
  for (const dt = new Date(start); dt <= today; dt.setDate(dt.getDate() + 1)) {
    const k = `${dt.getFullYear()}-${pad(dt.getMonth() + 1)}-${pad(dt.getDate())}`;
    const hit = sum.get(k);
    cells.push({ date: k, v: hit ? hit.v : 0, n: hit ? hit.n : 0 });
  }
  const nz = cells.map((c) => c.v).filter((x) => x > 0).sort((a, b) => a - b);
  const cut = (p) => (nz.length ? nz[Math.min(nz.length - 1, Math.floor(nz.length * p))] : 0);
  // 厂商自己给了分档阈值（Qoder 那条热力图接口就带 levels 四档）就照它的分，
  // 没给才按我们自己的分位数切——同一份数在官网和驾驶舱分色不同会被当成读数不一致
  const given = Array.isArray(t.levels) ? t.levels.filter((x) => typeof x === 'number') : [];
  return {
    cells, unit: t.unit || 'credits', total: numv(t.total),
    peak: numv(t.peak), peakDate: String(t.peak_date || ''),
    counted: nz.length, from: t.from || cells[0].date, to: t.to || cells[cells.length - 1].date,
    // 这份序列带不带"场会话"：Qoder 那条只有积分数，硬凑一句"0 场会话"就是假话
    hasSessions: t.days.some((d) => (numv(d.sessions) || 0) > 0),
    srcNote: String(t.source || ''),
    levels: given.length === 4 ? given : [cut(0.2), cut(0.4), cut(0.6), cut(0.8)],
    levelsFromVendor: given.length === 4,
  };
}

function heatLevel(v, lv) {
  if (!v || v <= 0) return 0;
  for (let i = 0; i < lv.length; i++) if (v <= lv[i]) return i + 1;
  return 5;
}

function trendWave(s, range) {
  const pts = s.cells.slice(-Math.min(range, s.cells.length));
  const W = 1000, H = 150, TOP = 12, BOT = 16;
  const max = Math.max(...pts.map((p) => p.v), 1);
  const dx = pts.length > 1 ? W / (pts.length - 1) : 0;
  const xy = pts.map((p, i) => [i * dx, H - BOT - (p.v / max) * (H - TOP - BOT)]);
  const clamp = (v, lo, hi) => Math.max(lo, Math.min(hi, v));
  // Catmull-Rom 转三次贝塞尔；控制点的 y 按相邻两点夹住，否则低谷会被冲过 0 变成负数
  let d = `M${xy[0][0].toFixed(2)},${xy[0][1].toFixed(2)}`;
  for (let i = 0; i < xy.length - 1; i++) {
    const p0 = xy[i - 1] || xy[i], p1 = xy[i], p2 = xy[i + 1], p3 = xy[i + 2] || p2;
    const lo = Math.min(p1[1], p2[1]), hi = Math.max(p1[1], p2[1]);
    const c1 = clamp(p1[1] + (p2[1] - p0[1]) / 6, lo, hi);
    const c2 = clamp(p2[1] - (p3[1] - p1[1]) / 6, lo, hi);
    d += ` C${(p1[0] + dx / 3).toFixed(2)},${c1.toFixed(2)}`
      + ` ${(p2[0] - dx / 3).toFixed(2)},${c2.toFixed(2)}`
      + ` ${p2[0].toFixed(2)},${p2[1].toFixed(2)}`;
  }
  const svg = svgNode('svg', {
    class: 'wave', viewBox: `0 0 ${W} ${H}`, preserveAspectRatio: 'none',
  });
  svg.appendChild(svgNode('path', {
    class: 'wfill', d: `${d} L${W},${H - BOT} L0,${H - BOT} Z`,
  }));
  svg.appendChild(svgNode('line', {
    class: 'wbase', x1: 0, y1: H - BOT, x2: W, y2: H - BOT,
  }));
  svg.appendChild(svgNode('path', {
    class: 'wline', d, 'vector-effect': 'non-scaling-stroke',
  }));
  const sel = S.trendSel || '';
  pts.forEach((p, i) => {
    const r = svgNode('rect', {
      class: `whit${p.date === sel ? ' on' : ''}`,
      x: (xy[i][0] - dx / 2).toFixed(2), y: 0,
      width: Math.max(dx, 2).toFixed(2), height: H,
    });
    const tip = svgNode('title', {});
    tip.textContent = `${p.date}　${nfmt(p.v)} ${utxt(s.unit)}`
      + (p.n ? ` · ${p.n} 场会话` : '');
    r.appendChild(tip);
    r.addEventListener('click', () => pickTrend(p));
    svg.appendChild(r);
  });
  // 选中那天钉一条竖导引线。不用 circle：preserveAspectRatio="none" 会把它拉成椭圆，
  // 而带 non-scaling-stroke 的竖线不管窗口多宽都是 1px
  const si = pts.findIndex((p) => p.date === sel);
  if (si >= 0) {
    svg.appendChild(svgNode('line', {
      class: 'wguide', x1: xy[si][0].toFixed(2), y1: TOP - 7,
      x2: xy[si][0].toFixed(2), y2: H - BOT, 'vector-effect': 'non-scaling-stroke',
    }));
  }
  const box = el('div', { class: 'wavewrap' },
    el('div', { class: 'wavey' },
      el('span', { style: `top:${TOP}px` }, nfmt(max)),
      // 大数取半档要取整：1,498,222,653 的半档写成 "276,721,718.5" 是噪声不是精度
      el('span', { style: `top:${(H - BOT) / 2}px` }, nfmt(max >= 1000 ? Math.round(max / 2) : max / 2)),
      el('span', { style: `top:${H - BOT}px` }, '0')),
    el('div', { class: 'wavec' }, svg),
    el('div', { class: 'wavex' },
      // 近一年这个窗口横跨两个日历年，只写 MM-DD 两头会看不出隔了多久
      el('span', {}, pts[0].date),
      el('span', {}, pts[pts.length - 1].date)));
  return box;
}

/* 点任意一天就把那天钉住：波形图与热力图共用一个选中态，两边同时高亮，
   数就常驻在图下面那行里。原来只有悬停 tooltip，鼠标一移开数就没了——
   她要的是能照着抄进笔记的值，不是划过才闪现的提示。 */
function pickTrend(p) {
  S.trendSel = S.trendSel === p.date ? '' : p.date;
  renderDetail();
  // 重建浮窗会把热力图的横向滚动位置打回最左（那里正好是一整片空格子），
  // 所以点完立刻再滚回最近这一个月
  const w = document.querySelector('#imodal .heatwrap');
  if (w) w.scrollLeft = w.scrollWidth - w.clientWidth;
}

function trendHeat(s) {
  const sel = S.trendSel || '';
  const weeks = [];
  for (let i = 0; i < s.cells.length; i += 7) weeks.push(s.cells.slice(i, i + 7));
  const grid = el('div', { class: 'heat' });
  const mons = el('div', { class: 'hmonths' });
  let lastMon = -1;
  weeks.forEach((wk, wi) => {
    const head = wk[0];
    const m = Number(String(head.date).slice(5, 7));
    // 只在月份第一次出现的那一列打标签，跟官网那种"每月一个刻度"一个画法
    if (m !== lastMon) {
      lastMon = m;
      mons.appendChild(el('span', { style: `left:${wi * 14}px` }, `${m}月`));
    }
    for (let r = 0; r < 7; r++) {
      const c = wk[r];
      // 最后一周不满七天：缺的那几格是"还没到的日子"，画成 0 消耗等于谎报，
      // 但也不能不占位——grid 是按列铺的，少一格后面整列都会错位
      if (!c) {
        grid.appendChild(el('i', { class: 'hc void' }));
        continue;
      }
      const cell = el('i', {
        class: `hc l${heatLevel(c.v, s.levels)}${c.date === sel ? ' sel' : ''}`,
        title: `${c.date}　${nfmt(c.v)} ${utxt(s.unit)}${c.n ? ` · ${c.n} 场会话` : ''}`,
      });
      cell.addEventListener('click', () => pickTrend(c));
      grid.appendChild(cell);
    }
  });
  return el('div', { class: 'heatwrap' }, el('div', { class: 'heatbox' }, mons, grid));
}
/* 热力图一年宽 739px、面板只有 500px 宽，默认停在最左等于先给她看一整片空格子。
   渲染完滚到最右，进来先看见的就是最近这一个月。
   一次 rAF 不够：浮窗重绘时这个元素可能还没接进文档，scrollWidth 量出来是 0，
   所以按帧重试到真能量出可滚的宽度为止（最多 8 帧，之后就不管了）。 */
function heatScrollToRecent(box, tries = 0) {
  const w = box.isConnected ? box.querySelector('.heatwrap') : null;
  const max = w ? w.scrollWidth - w.clientWidth : 0;
  if (max > 0) {
    w.scrollLeft = max;
    return;
  }
  if (tries < 8) requestAnimationFrame(() => heatScrollToRecent(box, tries + 1));
}

function trendCharts(q) {
  const t = q && q.trend;
  if (!t) return null;
  if (t.error) {
    return el('div', { class: 'qnote' }, `按天消耗读不到：${t.error}`);
  }
  const s = trendSeries(t);
  if (!s) return null;
  const range = S.trendRange || 90;
  const chips = TREND_RANGES.map((r) => {
    const b = el('button', { type: 'button', class: `tchip${r === range ? ' on' : ''}` },
      r === 365 ? '近一年' : `近 ${r} 天`);
    b.addEventListener('click', () => { S.trendRange = r; renderDetail(); });
    return b;
  });
  const box = el('div', { class: 'trend' });
  box.appendChild(el('div', { class: 'trhead' },
    el('b', {}, '消耗趋势'),
    el('span', { class: 'trsum' },
      `近一年合计 ${nfmt(s.total)} ${utxt(s.unit)} · ${s.counted} 天有消耗`
      + (s.peak ? ` · 峰值 ${s.peakDate} ${nfmt(s.peak)}` : '')),
    el('span', { class: 'trrange' }, ...chips)));
  const picked = s.cells.find((c) => c.date === (S.trendSel || ''));
  const pick = el('div', { class: 'trpick' },
    picked
      ? el('span', {}, `${picked.date.slice(0, 4)}年${Number(picked.date.slice(5, 7))}月${Number(picked.date.slice(8, 10))}日　`
        + `${nfmt(picked.v)} ${utxt(s.unit)}`
        + (picked.v <= 0 ? ' · 那天没有消耗记录'
          : s.hasSessions && picked.n ? ` · ${picked.n} 场会话` : ''))
      : el('span', { class: 'trhint' }, '点曲线或方格上的任意一天，这里会钉住那天的消耗'));
  if (picked) {
    const cl = el('button', { type: 'button', class: 'tchip' }, '清除');
    cl.addEventListener('click', () => { S.trendSel = ''; renderDetail(); });
    pick.appendChild(cl);
  }
  box.appendChild(pick);
  box.appendChild(trendWave(s, range));
  box.appendChild(trendHeat(s));
  // 滚到最右要等元素真的进了文档、有了布局再做，这里建好还没插入，scrollWidth 是 0
  requestAnimationFrame(() => heatScrollToRecent(box));
  const how = s.srcNote
    ? `按天分桶来自 ${s.srcNote}`
    : '归日按会话最后活动时间（跨天的会话整笔算到最后那天，不是逐笔流水）';
  const lv = s.levelsFromVendor
    ? '颜色分五档，阈值照厂商给的那份'
    : '颜色按这一年内有消耗的那些天分五档';
  box.appendChild(el('div', { class: 'trfoot' },
    el('span', {}, `热力图一格是一天；${lv}；${how}。`),
    el('span', { class: 'trlg' }, '少',
      ...[1, 2, 3, 4, 5].map((k) => el('i', { class: `hc l${k}` })), '多')));
  return box;
}

/* 账号设置里的「积分明细」整块：总览 + 每档一行 + 查看明细。版式照 WorkBuddy 官方。 */
function quotaPanel(i) {
  const q = i.quota;
  const groups = quotaRows(i);
  const wrap = el('div', { class: 'qpanel' });
  if (!groups.length) {
    // 一档都没有：明说读不到（带后端给的原因），不摆 0
    wrap.appendChild(el('div', { class: 'qempty', title: String(q.live_error || '').trim() }, quotaMiss(q)));
    const t0 = trendCharts(q);
    if (t0) wrap.appendChild(t0);
    return wrap;
  }
  const a = quotaAgg(groups);
  const up = a.up;
  const left = !up.mixed && numv(q.left) != null ? numv(q.left) : a.left;
  // 同 quotaBox：明细那份读数比汇总数旧时，分母跟最新的汇总数走，别拿旧分组求和配新剩余
  const aTotal = q.detail_stale && numv(q.total) != null ? numv(q.total) : a.total;
  const used = left != null && aTotal != null ? Math.max(0, aTotal - left) : null;
  const pct = quotaPct(aTotal, left);
  const notes = quotaCaveat(a, groups);
  // ① 顶部总览：大字总剩余（单位跟着这一档自己的 unit），右边按档列余额
  wrap.appendChild(el('div', { class: 'qsum' },
    el('div', { class: 'qbig' },
      el('span', { class: 'qcap' }, '总剩余'),
      el('b', {}, nfmt(left)),
      el('span', { class: 'qunit' }, up.main)),
    el('div', { class: 'qcats' },
      ...groups.map((g) => el('div', { class: 'qcat' },
        el('span', { class: 'qcname', title: g.scope ? `${g.name}：${g.scope}` : g.name },
          el('span', { class: 'qcnm' }, g.name), scopeTag(g)),
        el('b', {}, g.left == null ? '未给出'
          : `${nfmt(g.left)} ${utxt(g.unit)}`)))),
    // 有档没给总额时不摆比例：`已用 0 / 0　剩 0%` 和一条空进度条会被读成"额度用光了"
    pct != null && used != null
      ? el('div', { class: 'qused' },
        `已用 ${nfmt(used)} / ${nfmt(aTotal)} ${up.main}　剩 ${pctTxt(pct)}`) : null,
    pct != null ? el('div', { class: 'bar' }, el('i', { style: `width:${pct}%` })) : null,
    notes ? el('div', { class: 'qnote' }, notes) : null));
  // 明细这一层是从哪读的、几点读的：不写这一句，用户看到的就是"21 个包蜷成一行、
  // 到期时间未给出"，而完全不知道是被哪一份粗读数盖掉的、该点哪个按钮补回来
  const dq = detailStamp(q);
  if (dq) wrap.appendChild(el('div', { class: q.detail_stale ? 'qnote warn' : 'qnote' }, dq));
  if (String(q.web_error || '').trim()) {
    wrap.appendChild(el('div', { class: 'qnote warn' },
      `官网那次读数没成：${q.web_error}。逐笔到期只有官网给，`
      + '点「打开官网登录窗口」重新登一次，登上的话驾驶舱会自己把明细补回来。'));
  }
  // ② 每一档一行，能点进逐笔明细
  for (const g of groups) {
    const gp = quotaPct(g.total, g.left);
    const gu = utxt(g.unit);
    const noLeft = (g.items || []).filter((it) => it.left == null).length;
    // 到期话术：只有真给了日期才说"最近到期 / 下次刷新"，无限期和没给的说成"到期时间 …"
    const noEnd = !g.expires || g.expires === '—' || g.expires === EXP_NONE || g.expires === '无限期';
    const when = !noEnd
      ? el('span', { class: 'qgwhen' }, `${expiryLabel(g)} ${dtxt(g.expires)}`,
        g.days_left != null ? el('em', { class: expCls(g.days_left, q.stale) }, expText(g.days_left)) : null)
      : el('span', { class: 'qgwhen' }, `到期时间 ${g.expires || EXP_NONE}`);
    const foot = el('div', { class: 'qgfoot' },
      el('span', { class: 'qgcount' },
        `有效期内 ${g.live_count || 0} 笔` + (g.expired_count ? ` · 已过期 ${g.expired_count} 笔` : '')
        + (noLeft ? ` · ${noLeft} 笔官方只给了获得数` : '')),
      (g.items || []).length
        ? btn('查看明细 ›', 'sm', () => openQuotaDetail(i, g),
          '这一档里的每一笔资源包，各自什么时候到期、用了多少')
        : el('span', { class: 'qgcount' }, '官方接口只给汇总数，没有逐笔'));
    wrap.appendChild(el('div', { class: 'qgroup' },
      el('div', { class: 'qghead' },
        el('b', { class: 'qgname' }, g.name, scopeTag(g)),
        when,
        el('span', { class: 'qgstat' }, gp != null
          ? `已用 ${nfmt(g.used)} / ${nfmt(g.total)} ${gu}　剩余 ${pctTxt(gp)}`
          : g.left != null ? `剩 ${nfmt(g.left)} ${gu}（没给总额）` : '没给出数值')),
      gp != null ? el('div', { class: 'cbar', title: `剩 ${nfmt(g.left)} / 共 ${nfmt(g.total)} ${gu}` },
        el('b', { style: `width:${gp}%` })) : null,
      g.scope ? el('div', { class: 'qgscope' }, `这一档只用于：${g.scope}`) : null,
      foot));
  }
  const tc = trendCharts(q);
  if (tc) wrap.appendChild(tc);
  return wrap;
}
/* 查看明细：照官方那层「平台奖励积分明细」——有效期内 / 已过期 两个页签 + 逐笔列表。
   数据是刚读进来的那份，界面只负责按到期时间分堆。 */
let qdetail = null;
function openQuotaDetail(inst, group) {
  qdetail = {
    app: inst.app, name: inst.name, group: group.name, tab: 'live',
    at: (inst.quota || {}).fetched_at,
  };
  S.qdetail = qdetail;
  pendingQuota = null;
  $('qm-save').hidden = true;
  $('qmodal').hidden = false;
  renderQuotaDetail();
}
function closeQuotaDetail() {
  qdetail = null; S.qdetail = null;
  pendingQuota = null;
  $('qmodal').hidden = true;
  $('qm-save').hidden = true;
}
function renderQuotaDetail() {
  if (!qdetail) return;
  const inst = (S.data.instances || []).find((x) => x.app === qdetail.app && x.name === qdetail.name);
  if (!inst) { closeQuotaDetail(); return; }
  const g = quotaGroups(inst).find((x) => x.name === qdetail.group);
  if (!g) { closeQuotaDetail(); return; }
  const gu = utxt(g.unit);
  const stale = (inst.quota || {}).stale;
  $('qm-title').textContent = `${g.name}明细`;
  // 按到期时间升序：最早要作废的那一笔排最上面（没给日期 / 无限期的沉底）
  const byExp = (a, b) => expKey(a.expires_on).localeCompare(expKey(b.expires_on));
  const live = (g.items || []).filter((it) => !it.expired).sort(byExp);
  const dead = (g.items || []).filter((it) => it.expired).sort(byExp);
  const tabs = $('qm-tabs');
  clear(tabs);
  for (const [k, label, n] of [['live', '有效期内', live.length], ['dead', '已过期', dead.length]]) {
    const b = el('button', { class: `qtab${qdetail.tab === k ? ' on' : ''}` }, `${label} ${n}`);
    b.addEventListener('click', () => { qdetail.tab = k; renderQuotaDetail(); });
    tabs.appendChild(b);
  }
  const shown = qdetail.tab === 'live' ? live : dead;
  // 合计只加给了数的笔：厂商「获得明细」那种表没有逐笔剩余列，缺的笔不能当 0 加进来
  const lefts = shown.map((it) => it.left).filter((v) => v != null);
  const totals = shown.map((it) => it.total).filter((v) => v != null);
  const sumLeft = lefts.length ? lefts.reduce((s, v) => s + v, 0) : null;
  const sumTotal = totals.length ? totals.reduce((s, v) => s + v, 0) : null;
  const sum = $('qm-sum');
  clear(sum);
  if (shown.length) {
    const exps = shown.map((it) => it.expires_on).filter((x) => x && x !== EXP_NONE && x !== '无限期').sort();
    const nearest = qdetail.tab === 'live' ? exps[0] : exps[exps.length - 1];
    sum.append([
      `${shown.length} 笔`,
      sumLeft != null ? `合计剩 ${nfmt(sumLeft)} ${gu}` : '这一堆没有一笔给出剩余',
      sumLeft != null && sumTotal != null ? `共获得 ${nfmt(sumTotal)} ${gu}` : '',
      lefts.length < shown.length ? `其中 ${shown.length - lefts.length} 笔官方只给了获得数` : '',
      nearest ? `${qdetail.tab === 'live' ? '最近到期' : '最后到期'} ${dtxt(nearest)}` : '',
    ].filter(Boolean).join(' · '));
  } else {
    sum.textContent = qdetail.tab === 'live' ? '这一档已经没有在效期内的积分了' : '还没有过期的笔';
  }
  // scope 完整句子在明细层直接写出来：这一档到底只能花在什么上，别让人猜
  if (g.scope) sum.appendChild(el('span', { class: 'qscope' }, `这一档只用于：${g.scope}`));
  // 这份逐笔是从哪一层读来的、几点读的：只有官网逐包才给得出每一笔自己的到期日，
  // 本机那条接口给的是「主额度 + 附加包」两个汇总数——点开明细先看这一句就知道差在哪
  const dsrc = detailStamp(inst.quota || {});
  if (dsrc) sum.appendChild(el('span', { class: 'qscope' }, dsrc));
  const list = $('qm-list');
  clear(list);
  for (const it of shown) {
    // 缺数分三种说法：只给获得数（left 为 null）、只给剩余（total 为 null）、两个都没给。
    // 把 left 当 0 会写成「剩 0」，拿 total-left 反推会把刚到账的一笔说成已用完——都是假话
    const t = it.total, l = it.left, u = it.used;
    const iu = utxt(it.unit || g.unit);
    const p = quotaPct(t, l);
    list.appendChild(el('div', { class: 'qitem' },
      el('div', { class: 'qitop' },
        el('b', {}, it.label || g.name),
        it.detail ? el('span', { class: 'qtag' }, it.detail) : null,
        it.pool && it.pool !== it.label ? el('span', { class: 'qtag', title: it.pool }, it.pool.replace(/^资源包\s*/, '')) : null),
      el('div', { class: 'qimid' },
        el('span', {}, `到期时间 ${it.expires_on ? dtxt(it.expires_on) : EXP_NONE}`),
        it.days_left != null ? el('span', { class: expCls(it.days_left, stale) }, expText(it.days_left)) : null,
        el('span', { class: 'qistat' }, l == null && t == null ? '官方没给出数值'
          : l == null ? `获得 ${nfmt(t)} ${iu}`
            : t == null ? `剩 ${nfmt(l)} ${iu}（没给总额）`
              : u != null ? `已用 ${nfmt(u)} / ${nfmt(t)} ${iu}` : `剩 ${nfmt(l)} / ${nfmt(t)} ${iu}`)),
      p != null ? el('div', { class: 'cbar', title: `剩 ${nfmt(l)} / 共 ${nfmt(t)} ${iu}` },
        el('b', { style: `width:${p}%` })) : null));
  }
  if (!shown.length) list.appendChild(el('p', { class: 'qempty' }, '这一堆没有笔。'));
}

function identLine(i) {
  const id = i.identity || { state: 'none', label: '' };
  return el('div', { class: `ident ${id.state}` }, id.label);
}
// 「现在用的是哪个号」这一枚标记：能多开/靠 junction 换目录的产品看磁盘（⇄当前账号），
// 几个账号共用一份档案的产品看客户端登录态（⇄本机登录中）——磁盘记录不会随登录变化。
// 只有这个产品压根没有本机直读通道、登录态读不到时，才退回按档案认领关系标，并说明我们不知道登的是谁。
function accountMark(i, app, shared) {
  if (!shared) return i.swapped ? el('span', { class: 'swapmark' }, '⇄ 当前账号') : null;
  if (i.logged_in) return el('span', { class: 'swapmark' }, '⇄ 本机登录中');
  const mv = (app && app.machine && app.machine.value) || {};
  if (app && app.local_quota && !mv.unsupported) return null;
  return i.swapped
    ? el('span', {
      class: 'swapmark',
      title: '这个程序没有本机直读通道，读不到现在登的是谁，只能按档案认领关系标出来',
    }, '⇄ 当前账号') : null;
}
// 卡片上的数属于谁 ≠ 本机现在登的是谁。以前不一致时后端直接把新读数挡回去，
// 界面就永远停在旧号身上，看起来像"刷新坏了"。现在把这件事写在卡片上并给出动作。
function mismatchLine(i, app) {
  const q = i.quota || {};
  if (!q.bound_mismatch) return null;
  const mv = (app && app.machine && app.machine.value) || {};
  if (i.released) {
    return el('div', { class: 'ident' },
      `历史号：这份档案现在登的是「${mv.account || '另一个号'}」，这张卡留着看它当时的额度`
      + `${i.released_to ? `（已让位给「${i.released_to}」）` : ''}。`);
  }
  const n = el('div', { class: 'ident mismatch' },
    `本机现在登的是「${mv.account || '另一个号'}」，这张卡的数是「${q.local_account || '?'}」的`
    + `${q.fetched_at ? `（读数于 ${stampTime(q.fetched_at)}）` : ''}。`);
  n.append(btn('改绑到当前登录', 'sm', async () => {
    const r = await invoke('action', { payload: { action: 'rebind_instance', app: i.app, name: i.name } })
      .catch((e) => ({ ok: false, msg: String(e) }));
    toast(r.msg || (r.ok ? '已改绑' : '改绑失败'), !!r.ok);
    if (r.ok) load();
  }, '把这张卡的名字与绑定的 userId 换成机器上现在登录的那个号，并按当前读数重写额度'));
  n.append(btn(app && app.shared_profile ? '启动客户端' : '切换到此账号', 'sm',
    () => swapSetAndLaunch(i.app, i.name),
    app && app.shared_profile
      ? '打开这个程序，在里面退出登录再登这张卡的号。共用一份档案的产品切换账号就在客户端里做：不关程序、不搬文件。'
      : '先把默认档案换回这个账号，再打开程序——换过来之后读到的就是它的真数'));
  return n;
}

function renderCards(d) {
  const app = d.apps.find((a) => a.id === S.cur);
  const wrap = $('cards');
  clear(wrap);
  // 启动体检：换号记录和磁盘对不上、默认档案入口成断链这类，程序自己修了什么必须当场说出来。
  // 不按当前选中的产品过滤——修的是别的程序的话，更应该让你看见。
  for (const msg of (d.selfcheck || [])) {
    wrap.appendChild(el('div', { class: 'hintline' }, '启动体检 · ' + msg));
  }
  const list = d.instances.filter((i) => i.app === S.cur);
  if (!list.length) {
    wrap.appendChild(el('div', { class: 'empty' },
      '还没有账号。点右上「＋ 添加账号」：选登录方式 → 打开客户端登录 → 认出登的是谁并填好基本信息 → 决定档案要不要带过来、换号还是多开 → 读额度 → 确认建卡。'));
    return;
  }
  for (const i of list) wrap.appendChild(card(i, app));
}

// 数据来源标签：这个账号打开时会不会带着你原来的对话与记忆
const ORIGIN_LABEL = {
  adopt: '沿用本机原有档案',
  copy: '继承已有账号',
  fresh: '全新（无历史）',
};
function originText(i) {
  if (!i.origin) return '';
  const t = ORIGIN_LABEL[i.origin] || i.origin;
  const from = i.seeded_from
    ? i.seeded_from.replace(/[\\/]+$/, '').split(/[\\/]/).slice(-2)[0]
    : '';
  return i.origin === 'copy' && from ? `${t} · 来自 ${from}` : t;
}

function kindLabel(kind) {
  return kind === 'cli' ? '命令行' : (kind === 'plain' ? '桌面端（不可隔离）' : '桌面端');
}

function card(i, app) {
  const canIso = !!(app && app.isolatable);
  // 共用一份档案的产品：所有账号的数据都在默认位置那一份里，切换账号是客户端里的
  // 退出登录/重新登录，磁盘上没有东西可换——这种卡一律不许走 swap_set
  const shared = !!(app && app.shared_profile);
  // 主按钮 = 这个账号「现在能不能开、怎么开」。只有一个，避免"点了不知道会发生什么"
  // 「窗口都关了、进程还挂着」不算开着：Electron 的主进程被结束后，崩溃上报那类子进程
  // 还会在进程表里赖上一分钟左右，那期间把主按钮留在「■ 结束」上，看着就像按钮卡住了。
  let primary;
  if (i.running) {
    primary = btn('■ 结束', 'sm', () => run('kill', { app: i.app, name: i.name }),
      canIso
        ? '只结束这个账号自己的窗口，不影响同程序其它实例；未保存的内容会被强制中断'
        : '该程序不能多开，这会结束整个程序的窗口；未保存的内容会被强制中断');
  } else if (canIso) {
    primary = btn('▶ 启动', 'primary sm', () => run('launch', { app: i.app, name: i.name }),
      '用这个账号自己的数据目录开一个新窗口，可以和其它账号同时开着');
  } else if (shared) {
    // 共用一份档案的产品：切号＝把那个号存过的登录态放回档案位置。
    // 没存过的号只能先去登一次（登进来驾驶舱会自动收下那份登录态），我们绝不替她猜密码。
    if (i.logged_in) {
      primary = btn('▶ 打开', 'primary sm', () => openClient(i.app, i.name),
        `这份档案现在登的就是「${i.name}」，打开就是它。不用换号，也不会动任何进程。`);
    } else if (app.auth_switch && i.auth_pack) {
      primary = btn('⇄ 切换到此账号', 'primary sm', () => authSwitch(i.app, i.name),
        `把「${i.name}」存过的登录态写回 ${i.app_name} 的档案位置，然后自动重开客户端。\n`
        + `会重启一次 ${i.app_name}（它只在启动时读登录文件），当前这个号的登录态在写之前会先收好，随时切得回来。`);
    } else {
      primary = btn('▶ 启动客户端', 'primary sm', () => openClient(i.app, i.name),
        `打开 ${i.app_name}，在里面退出登录、再登「${i.name}」。\n`
        + (app.auth_switch
          ? '登进来之后驾驶舱会自动收下这个号的登录态（约 45 秒内），以后来回切都是一键。'
          : '这类产品所有账号共用一份档案，切换就在客户端里完成：驾驶舱不搬文件、也不会关掉你的程序。'));
    }
  } else if (i.swapped) {
    primary = btn('▶ 启动', 'primary sm', () => run('launch_app', { app: i.app, name: i.name }),
      `默认入口现在挂的就是「${i.name}」，打开就是它`);
  } else {
    // 不能多开的产品，"打开这个账号"必须先换号——合成一个动作，省掉"先点设为当前再点启动"两步
    primary = btn('▶ 切到本账号并打开', 'primary sm', () => swapSetAndLaunch(i.app, i.name),
      `先把默认档案切到「${i.name}」（junction，零额外空间、随时可还原），再打开该程序。`
      + (i.app_running ? '\n注意：该程序有窗口在跑，需要先关掉——换号会提示你。' : ''));
  }
  // 次按钮：只有"现在是当前账号"才需要"还原默认"；可多开的产品才留「设为当前」。
  // 原地账号本来就是这台机器的默认档案，磁盘上没有让位备份可还原——摆这颗按钮只会骗人
  const swapBtn = !app || !app.swap_ok || i.in_place ? null
    : i.swapped
      ? btn('还原默认', 'danger sm', () => run('swap_clear', { app: i.app }),
        '解除换号，默认入口换回你的原档案（需先关闭该程序窗口）。账号目录本身保留，随时可再切回来')
      : (canIso
        ? btn('设为当前', 'sm', () => swapSet(i.app, i.name),
          '这个程序本来就能多开，通常不需要换号；只有在你想让开始菜单/任务栏也直接打开这个账号时才用')
        : null);
  // 一颗按钮读全部：能走官网就走官网（只有那里给得出每个积分包各自的到期时间），
  // 读不到才退回本机/服务端汇总。以前两条路各摆一颗按钮，她的预期是"点一次就都该有"
  const pull = app && (app.local_quota || app.web_quota)
    ? btn('↻ 刷新额度', 'sm', () => refreshQuota(i.app, i.name),
      '一次把这个号的额度读全：每个积分包各剩多少、各自什么时候作废、已用多少、每日消耗。\n'
      + '读的是这个号自己的官网会话，跟这台机器的客户端现在登的是谁无关（一个号只需登一次）。\n'
      + '官网读不到时会退回本机/服务端汇总，并当场说清缺了哪一层。')
    : null;
  // 能多开的产品（ZCode 这类）主按钮留给「启动」，一键切号作为次按钮放在旁边：
  // 它们的登录态同样是全机一份，多开只是窗口多开，账号还是那一个
  const jumpBtn = canIso && app && app.auth_switch && i.auth_pack && !i.logged_in
    ? btn('⇄ 切换到此账号', 'sm', () => authSwitch(i.app, i.name),
      `把「${i.name}」存过的登录态写回 ${i.app_name}，然后自动重开它。\n`
      + '这个程序的登录态是全机一份，多开窗口并不会多开账号；当前号的登录态会在写之前先收好。')
    : null;
  // 残留进程只在真有残留的那一瞬出现：平时不占卡片位置，出现时正好是需要它的时刻
  const reap = i.resident
    ? btn(`■ 清残留 ×${i.procs}`, 'sm', () => run('kill', { app: i.app, name: i.name }),
      '窗口已经关了，这几个进程还挂在系统里（Electron 的崩溃上报、工具进程常见，一般一分钟内自己就退）。'
      + '不影响你重新打开，但会挡住换号、改名和归位——要立刻腾出来就点这一下')
    : null;
  const detail = btn('账号设置 ⚙', 'sm', () => openDetail(i.app, i.name),
    '打开这个账号的设置：账号信息 → 额度与用量 → 运行与换号 → 目录与数据 → 凭据与反代 → 删除账号');
  // 删除放在这一行最外侧：它属于"整个账号"的级别，不该藏在设置里绕一圈
  const delBtn = btn('删除', 'sm del', () => del(i.app, i.name), '删掉这个账号的数据目录（含登录状态）。删除前会自动打一个 zip 备份');

  // 额度那一块单独可点：它讲的是"还剩多少、什么时候作废"，点它就该直接落到额度页，
  // 不用先落在设置首页再去左边找分组
  const qbox = quotaBox(i);
  qbox.classList.add('jump');
  qbox.setAttribute('title', '看每一档额度的逐笔到期明细');
  qbox.addEventListener('click', (e) => {
    e.stopPropagation();
    openDetail(i.app, i.name, 'quota');
  });
  const hasQuota = quotaRows(i).length || numv(i.quota.total) != null || numv(i.quota.left) != null;

  const node = el('div', { class: `card${i.running ? ' running' : (i.resident ? ' resident' : '')}`, tabindex: 0, role: 'button',
    'aria-label': `${i.name} 账号设置` },
    el('div', { class: 'top' },
      el('div', {},
        el('div', { class: 'nm' }, i.name,
          accountMark(i, app, shared),
          checkinMark(i),
          i.released && !i.logged_in ? el('span', { class: 'swapmark off' }, '已让位') : null),
        el('div', { class: 'meta' },
          `${i.app_name} · ${kindLabel(i.kind)} · 建于 ${i.created}`),
        originText(i) ? el('div', { class: 'orig', title: i.seeded_from || '' }, originText(i)) : null,
        identLine(i),
        mismatchLine(i, app),
        i.usage && i.usage.total != null
          ? el('div', { class: 'usage', title: `从本地会话日志汇总 · 入 ${fmtTok(i.usage.input)} / 出 ${fmtTok(i.usage.output)} / 缓存 ${fmtTok(i.usage.cached)} / 推理 ${fmtTok(i.usage.reasoning)}` },
              `用量 ${fmtTok(i.usage.total)} tokens`)
          : null),
      el('div', { class: 'status', title: i.running
        ? `该账号有 ${i.procs} 个进程在跑，窗口开着`
        : (i.resident
          ? `窗口都关了，但还有 ${i.procs} 个进程驻留在后台：不算开着，会挡住换号和改名`
          : (i.app_running
            ? (shared
              ? '该程序有窗口在跑，登的是别的号——点「启动客户端」，在里面退出登录再登这个号就行，不用关程序'
              : '该程序有窗口在跑，但登的不是这个账号——要切过来先点「切到本账号并打开」')
            : '该账号当前没有窗口在运行')) },
        el('span', { class: i.resident ? 'dot idle' : 'dot' }),
        i.running ? (i.kind === 'cli' ? '运行中' : `运行中 ×${i.procs}`)
          : (i.resident ? `后台驻留 ×${i.procs}` : '未运行'))),
    qbox,
    el('div', { class: 'acts' }, primary, reap, jumpBtn, swapBtn, pull, detail,
      el('span', { class: 'sp' }), delBtn),
    el('div', { class: 'peek' },
      hasQuota
        ? '点上面这块额度 → 逐笔到期明细；点别处 → 账号设置'
        : '点卡片空白处 → 账号设置；额度从「额度与用量」里读或贴截图识别'));
  // 卡片表头/空白处 = 账号设置首页；只有额度那块跳到额度页
  node.addEventListener('click', (e) => {
    if (e.target.closest('button')) return;
    openDetail(i.app, i.name);
  });
  node.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); openDetail(i.app, i.name); }
  });
  return node;
}

async function restoreConfirm(app, name) {
  if (await askConfirm(`用最近一次备份覆盖「${name}」的当前数据？\n备份之后新产生的改动会丢失，且需先关闭该账号窗口。`,
    { danger: true, okText: '覆盖恢复' })) {
    run('restore', { app, name });
  }
}
async function renameAccount(app, name) {
  const to = await askText(`把账号「${name}」改名为：`, name, { title: '改账号名', okText: '改名' });
  if (!to || to.trim() === name) return;
  const r = await run('rename_instance', { app, name, to: to.trim() });
  if (r.ok) {
    S.cur = app;
    if (S.detail && S.detail.app === app && S.detail.name === name) S.detail.name = to.trim();
  }
}

/* ---------------- 福利活动：各家限时免费 / 每日赠送 ----------------
   这类信息官方既没接口也没日历，全靠手动记一条；到期日一过就收进"已过期"不再提示。 */
function perkForm(item) {
  const d = S.data;
  const f = { ...(item || { app: S.cur || '', title: '', detail: '', ends_on: '', daily: false, url: '' }) };
  const opts = [{ id: '', name: '（不绑定某个程序）' }, ...d.apps.map((a) => ({ id: a.id, name: a.name }))];
  const sel = el('select', {}, ...opts.map((o) => el('option', { value: o.id }, o.name)));
  sel.value = f.app || '';
  sel.addEventListener('change', () => { f.app = sel.value; });
  const inp = (key, ph, type) => {
    const i = el('input', { type: type || 'text', placeholder: ph || '', value: f[key] ?? '' });
    i.addEventListener('input', () => { f[key] = i.value; });
    return i;
  };
  const daily = el('input', { type: 'checkbox' });
  daily.checked = !!f.daily;
  daily.addEventListener('change', () => { f.daily = daily.checked; });
  return el('div', { class: 'perkform' },
    el('div', { class: 'prow' }, el('label', {}, '程序'), sel, el('label', {}, '标题'),
      inp('title', '如：每天免费用 Qwen3.8-Max-Preview')),
    el('div', { class: 'prow' }, el('label', {}, '说明'),
      inp('detail', '如：0 倍率 · 每天 00:05 重置额度'),
      el('label', {}, '到期日'), inp('ends_on', '2026-10-19', 'text'),
      el('label', { class: 'chk' }, daily, '每天重复'),
      el('label', {}, '出处'), inp('url', 'https://… 可选')),
    el('div', { class: 'prow' },
      el('span', { class: 'kv' }, '截图识别：这个表单开着时按 Ctrl+V 贴截图'),
      pasteHint('perk'),
      btn('保存', 'primary sm', async () => {
        if (!f.title.trim()) { toast('标题不能为空', false); return; }
        S.perkForm = null;
        run('perk_save', { perk: f });
      }),
      btn('取消', 'sm', () => { S.perkForm = null; renderPerks(S.data); })));
}

function renderPerks(d) {
  const host = $('perks-list');
  if (!host) return;
  // 正在填表时 4 秒轮询别重绘，否则输入到一半的字会被吞掉
  if (S.perkForm && host.querySelector('.perkform')) return;
  clear(host);
  const addBtn = btn('＋ 添加福利', S.perkForm ? 'sm' : 'primary sm',
    () => { S.perkForm = { item: null }; renderPerks(S.data); });
  const all = d.perks || [];
  const live = all.filter((p) => p.live);
  const dead = all.filter((p) => !p.live);
  const rank = (p) => (p.app === S.cur ? 0 : 1);
  live.sort((a, b) => rank(a) - rank(b) || (a.days_left ?? 9e9) - (b.days_left ?? 9e9));
  const line = (p, dim) => el('div', { class: `perk${dim ? ' dim' : ''}` },
    el('span', { class: 'pag' }, p.app_name || '通用'),
    el('b', {}, p.title),
    p.detail ? el('span', { class: 'pd' }, p.detail) : null,
    p.url ? el('a', { href: p.url, target: '_blank', rel: 'noreferrer' }, '出处') : null,
    el('span', { class: 'pspacer' }),
    el('span', { class: dim ? 'exp danger' : 'exp' }, !p.live
      ? '已过期'
      : p.daily ? '每天' : p.days_left != null ? `还剩 ${p.days_left} 天` : (p.ends_on ? dtxt(p.ends_on) : '长期')),
    btn('编辑', 'sm', () => { S.perkForm = { item: p }; renderPerks(S.data); }),
    btn('删除', 'sm', async () => {
      if (!await askConfirm(`删掉这条福利「${p.title}」？`, { danger: true, okText: '删除' })) return;
      S.perkForm = null;
      run('perk_delete', { id: p.id });
    }));
  if (!all.length) {
    host.append(el('div', { class: 'perk-empty' },
      el('p', {}, '还没有记录。把各家送的免费额度记一条，例如「Qoder：每天免费用 Qwen3.8-Max-Preview」「WorkBuddy AI：DeepSeek-V4 flash 倍率 0」——这类活动官方既没接口也没日历，只能手动登记。'),
      addBtn));
  } else {
    host.append(el('div', { class: 'perkhead' },
      el('span', {}, live.length ? `进行中的福利 ${live.length} 条` : '没有进行中的福利'),
      addBtn));
    live.forEach((p) => host.append(line(p, false)));
    if (dead.length) {
      host.append(el('h3', { class: 'subsec' }, '已过期的记录'));
      dead.forEach((p) => host.append(line(p, true)));
    }
  }
  if (S.perkForm) host.append(perkForm(S.perkForm.prefill || S.perkForm.item));
}

function renderProcs(d) {
  const tb = $('procs');
  clear(tb);
  if (!d.procs.length) {
    tb.appendChild(el('tr', { class: 'none' }, el('td', { colSpan: 4 }, '当前没有运行中的 Agent 进程')));
    return;
  }
  for (const p of d.procs) {
    const pids = p.pids.split(',').map(Number).filter(Boolean);
    tb.appendChild(el('tr', {},
      el('td', { class: 't', title: p.app }, p.app),
      el('td', { class: 't', title: p.detail }, p.detail),
      el('td', { title: p.pids }, p.pids),
      el('td', {}, btn('结束全部', 'danger sm', () => killAll(pids, p.app),
        '结束该程序的整个进程树，包含你正在使用的窗口；只想关一个账号请用卡片上的「■ 结束」'))));
  }
}

function renderProxy(d) {
  $('p-up').value = d.proxy.upstream || '';
  $('p-st').value = d.proxy.strategy || 'round-robin';
  const b = $('p-btn');
  clear(b);
  b.append(d.proxy.running ? '停止反代' : '启动反代');
  b.className = `btn ${d.proxy.running ? 'danger' : 'primary'}`;
  $('p-pool').querySelector('span').textContent =
    `token 池 ${d.proxy.pool.length}：${d.proxy.pool.join('、') || '空'}`;

  renderGateway(d.proxy);

  const tb = $('plogs');
  clear(tb);
  if (!d.proxy.logs.length) {
    tb.appendChild(el('tr', { class: 'none' }, el('td', { colSpan: 8 }, '暂无请求记录')));
    return;
  }
  for (const l of d.proxy.logs) {
    tb.appendChild(el('tr', {},
      el('td', {}, l.ts), el('td', {}, l.method),
      el('td', { title: l.path }, l.path), el('td', { title: l.model }, l.model),
      el('td', { class: `s${String(l.status)[0]}` }, l.status),
      el('td', { class: 't', title: l.account }, l.account),
      el('td', { class: 't', title: l.pool || '' }, l.pool || '—'),
      el('td', { class: 't', title: l.via || '未识别的 Key（走全部池子）' },
        l.via || el('span', { class: 'muted' }, '未识别'))));
  }
}

/* ---------------- 反代编制：池子（谁进哪个池）+ 分发 Key（谁能用哪些池） ----------------
   模型是三段式：来源（某个 Agent 的某个账号 / 台账里的某个 Key）→ 池子 → 分发 Key。
   请求带着分发 Key 进来，反代就只在它被授权的池子里挑上游，并把 Authorization 换成该上游自己的凭据。 */
function renderGateway(p) {
  const host = $('gwb');
  if (!host) return;
  clear(host);
  const pools = p.pools || [];
  const dist = p.dist || [];
  const pname = (id) => (pools.find((x) => x.id === id) || {}).name || '（池子已删）';

  /* ---- 池子 ---- */
  host.appendChild(el('div', { class: 'gwhead' },
    el('b', {}, '池子'),
    el('span', { class: 'gwhint' }, '哪个 Agent 的哪个账号 → 进哪个池'),
    el('span', { class: 'sp' }),
    btn('＋ 新建池子', 'sm', () => openPoolModal(null), '把要参与分流的账号 / Key 归到一个池里')));

  if (!pools.length) {
    host.appendChild(el('div', { class: 'gwempty' },
      '还没有池子。没有池子时反代退回老行为：所有登记过 Token 的账号一起轮询，谁都能用。'));
  } else {
    const wrap = el('div', { class: 'gwgrid' });
    for (const g of pools) {
      const total = (g.members || []).length;
      const live = g.live ?? 0;
      const labels = g.labels || [];
      wrap.appendChild(el('div', { class: `gwcard${g.enabled === false ? ' off' : ''}` },
        el('div', { class: 'gwcardhead' },
          el('b', {}, g.name || '未命名'),
          g.enabled === false ? chip('停用', 'expired') : null,
          el('span', { class: 'sp' }),
          el('span', { class: `live${live ? '' : ' bad'}`, title: '实际可用上游数 / 勾选的成员数' },
            `可用 ${live}/${total}`)),
        el('div', { class: 'gwcardtags' }, labels.length
          ? labels.map((t) => el('span', { class: 'tag' }, t))
          : el('span', { class: 'muted' }, total
            ? '勾进来的成员目前都不可用（账号没 Token / 记录已被删）'
            : '这个池还没勾成员')),
        el('div', { class: 'gwcardacts' },
          btn('改', 'sm', () => openPoolModal(g), '改池子名、增删成员'),
          btn('删', 'danger sm', async () => {
            if (await askConfirm(`删除池子「${g.name}」？\n引用它的分发 Key 会自动去掉这个池。`,
              { danger: true, okText: '删除池子' })) {
              run('proxy_pool_delete', { id: g.id });
            }
          }, '删掉这个池子'))));
    }
    host.appendChild(wrap);
  }

  /* ---- 分发出去的 API Key ---- */
  host.appendChild(el('div', { class: 'gwhead sep' },
    el('b', {}, '分发出去的 API Key'),
    el('span', { class: 'gwhint' }, '每个 Key 只放行它被授权的那几个池'),
    el('span', { class: 'sp' }),
    btn('＋ 生成分发 Key', 'primary sm', () => openDistModal(null), '生成一个能对外分发的 key，指定它只能走哪些池')));

  if (!dist.length) {
    host.appendChild(el('div', { class: 'gwempty' },
      '还没有分发 Key。生成一个（起个名字标明谁在用），勾选允许走哪些池，再把它填到客户端的 API Key 位置。'));
    return;
  }
  const list = el('div', { class: 'gwgrid' });
  for (const k of dist) {
    const ids = k.pools || [];
    list.appendChild(el('div', { class: `gwcard${k.enabled ? '' : ' off'}` },
      el('div', { class: 'gwcardhead' },
        el('b', {}, k.name || '未命名'),
        k.enabled ? chip('在用', 'swap') : chip('停用', 'expired'),
        el('span', { class: 'sp' }),
        el('span', { class: 'live', title: '累计调用次数 · 最近一次落到哪个上游' },
          `${k.calls || 0} 次${k.last_account ? ` · ${k.last_account}` : ''}`)),
      el('div', { class: 'gwkey' },
        el('code', { class: 'mono', id: `gwkey-${k.id}` }, k.masked || '—'),
        btn('显示', 'sm', () => revealDist(k), '显示完整 Key（列表默认打码）'),
        btn('复制', 'sm', () => copyDist(k), '复制完整 Key 到剪贴板')),
      el('div', { class: 'gwcardtags' },
        el('span', { class: 'tag' }, ids.length
          ? `授权：${ids.map(pname).join('、')}`
          : '授权：全部池子'),
        k.last_used ? el('span', { class: 'tag' }, `最近 ${stampTime(k.last_used)}`) : null),
      el('div', { class: 'gwcardacts' },
        btn(k.enabled ? '停用' : '启用', 'sm', () => run('proxy_dist_toggle', { id: k.id }),
          k.enabled ? '停用后用它的人会立刻被拒（403）' : '重新放行'),
        btn('改', 'sm', () => openDistModal(k), '改名字、授权池子、状态'),
        btn('删', 'danger sm', async () => {
          if (await askConfirm(`删除分发 Key「${k.name}」？\n正在用它的人会立刻被拒。不可恢复。`,
            { danger: true, okText: '删除' })) {
            run('proxy_dist_delete', { id: k.id });
          }
        }, '彻底删掉这个分发的 key'))));
  }
  host.appendChild(list);
}

async function revealDist(k) {
  const node = document.getElementById(`gwkey-${k.id}`);
  if (!node) return;
  if (node.dataset.shown === '1') {           // 再点一次收起
    node.textContent = k.masked || '—';
    node.dataset.shown = '0';
    return;
  }
  const r = await invoke('action', { payload: { action: 'proxy_dist_reveal', id: k.id } });
  if (!r.ok) { toast(r.msg, false); return; }
  node.textContent = r.msg;
  node.dataset.shown = '1';
}

async function copyDist(k) {
  const r = await invoke('action', { payload: { action: 'proxy_dist_reveal', id: k.id } });
  if (!r.ok) { toast(r.msg, false); return; }
  try {
    await navigator.clipboard.writeText(r.msg);
    toast(`已复制「${k.name || '分发 Key'}」到剪贴板`);
  } catch (e) {
    toast(`复制失败：${e}`, false);
  }
}

/* 某个账号被归进了哪些池子（账号设置里提示反代归属用） */
function poolsOfInstance(app, name) {
  const src = `instance:${app}/${name}`;
  return (S.data?.proxy?.pools || [])
    .filter((g) => (g.members || []).some((m) => m.src === src))
    .map((g) => g.name);
}

/* ---- 池子编辑弹窗 ---- */
let poolEditing = null;
function openPoolModal(g) {
  poolEditing = {
    id: g?.id || '', name: g?.name || '',
    enabled: g?.enabled !== false,
    // 深拷一份成员：改弹窗里的勾选不该立刻影响外面已渲染的卡片
    members: (g?.members || []).map((m) => ({ ...m })),
  };
  $('gp-title').textContent = g ? `编辑池子 · ${g.name || '未命名'}` : '新建池子';
  $('gp-name').value = poolEditing.name;
  $('gp-en').value = poolEditing.enabled ? '1' : '0';
  renderPoolMembers();
  $('gpmodal').hidden = false;
  $('gp-name').focus();
}
function closePoolModal() { $('gpmodal').hidden = true; poolEditing = null; }

function renderPoolMembers() {
  const host = $('gp-members');
  clear(host);
  const mem = (S.data?.proxy?.members) || { instances: [], keys: [] };
  const has = (src) => (poolEditing.members || []).some((m) => m.src === src);
  const baseOf = (src) => ((poolEditing.members || []).find((m) => m.src === src) || {}).base_url || '';
  const toggle = (src, on) => {
    const list = poolEditing.members || (poolEditing.members = []);
    const i = list.findIndex((m) => m.src === src);
    if (on && i < 0) list.push({ src, base_url: '', enabled: true });
    if (!on && i >= 0) list.splice(i, 1);
    updatePoolCount();
  };
  const setBase = (src, v) => {
    const m = (poolEditing.members || []).find((x) => x.src === src);
    if (m) m.base_url = v;
  };

  if (!mem.instances.length && !mem.keys.length) {
    host.appendChild(el('div', { class: 'gwempty' },
      '还没有可选的来源：先在左侧给某个 Agent 建个账号并把 Token 填上，或在「API 与额度」里登记一个 Key。'));
    return;
  }
  if (mem.instances.length) {
    host.appendChild(el('div', { class: 'gpg' }, 'Agent 账号'));
    for (const m of mem.instances) {
      host.appendChild(poolRow({
        src: m.src,
        label: `${m.app_name} / ${m.instance}`,
        bad: !m.has_token ? '该账号还没登记 Token，勾了也用不上' : '',
        checked: has(m.src),
        base: baseOf(m.src),
        onToggle: (v) => toggle(m.src, v),
        onBase: (v) => setBase(m.src, v),
      }));
    }
  }
  if (mem.keys.length) {
    host.appendChild(el('div', { class: 'gpg' }, 'API 台账里的 Key'));
    for (const m of mem.keys) {
      host.appendChild(poolRow({
        src: m.src,
        label: `${m.site_name} / ${m.label || '（未命名）'}`,
        sub: m.masked || '',
        checked: has(m.src),
        base: baseOf(m.src),
        onToggle: (v) => toggle(m.src, v),
        onBase: (v) => setBase(m.src, v),
      }));
    }
  }
  updatePoolCount();
}

function poolRow(o) {
  const cb = el('input', { type: 'checkbox', class: 'gpmem-cb' });
  cb.checked = !!o.checked;
  cb.addEventListener('change', () => o.onToggle(cb.checked));
  const name = el('span', { class: 'gpmem-name', title: o.bad || o.label }, o.label);
  name.addEventListener('click', () => { cb.checked = !cb.checked; o.onToggle(cb.checked); });
  const base = el('input', {
    class: 'gpmem-base', placeholder: '上游地址（留空＝全局）',
    autocomplete: 'off', spellcheck: 'false',
  });
  base.value = o.base || '';
  base.addEventListener('input', () => o.onBase(base.value.trim()));
  return el('div', { class: `gpmem${o.bad ? ' warn' : ''}` },
    cb, name,
    o.sub ? el('code', { class: 'mono gpmem-sub' }, o.sub) : null,
    o.bad ? el('span', { class: 'tag expired', title: o.bad }, '不可用') : null,
    base);
}

function updatePoolCount() {
  const n = (poolEditing?.members || []).length;
  const c = $('gp-count');
  if (c) c.textContent = n ? `已勾 ${n} 个来源` : '没勾来源 → 保存后是个空池';
}

function savePoolModal() {
  const item = {
    id: poolEditing.id || '',
    name: $('gp-name').value.trim(),
    enabled: $('gp-en').value === '1',
    members: poolEditing.members || [],
  };
  if (!item.name) { toast('池子名不能为空', false); return; }
  run('proxy_pool_upsert', { item });
  closePoolModal();
}

/* ---- 分发 Key 编辑弹窗 ---- */
let distEditing = null;
function openDistModal(k) {
  distEditing = { id: k?.id || '', name: k?.name || '', enabled: k?.enabled !== false, pools: [...(k?.pools || [])] };
  $('gd-title').textContent = k ? `编辑分发 Key · ${k.name || '未命名'}` : '生成分发 Key';
  $('gd-name').value = distEditing.name;
  $('gd-en').value = distEditing.enabled ? '1' : '0';
  $('gd-out').hidden = true;
  clear($('gd-out'));
  renderDistPools();
  $('gdmodal').hidden = false;
  $('gd-name').focus();
}
function closeDistModal() { $('gdmodal').hidden = true; distEditing = null; }

function renderDistPools() {
  const host = $('gd-pools');
  clear(host);
  const pools = (S.data?.proxy?.pools) || [];
  if (!pools.length) {
    host.appendChild(el('div', { class: 'gwempty' }, '还没有池子。先新建一个池子，再回来给它授权。'));
    return;
  }
  host.appendChild(el('div', { class: 'gpg' }, '允许走哪些池子（一个都不勾 = 全部启用的池子）'));
  for (const g of pools) {
    const cb = el('input', { type: 'checkbox', class: 'gpmem-cb' });
    cb.checked = distEditing.pools.includes(g.id);
    cb.addEventListener('change', () => {
      const list = distEditing.pools;
      const i = list.indexOf(g.id);
      if (cb.checked && i < 0) list.push(g.id);
      if (!cb.checked && i >= 0) list.splice(i, 1);
    });
    const name = el('span', { class: 'gpmem-name' }, g.name || '未命名');
    name.addEventListener('click', () => { cb.checked = !cb.checked; cb.dispatchEvent(new Event('change')); });
    host.appendChild(el('div', { class: 'gpmem' },
      cb, name,
      el('span', { class: 'tag' }, `可用 ${g.live ?? 0}/${(g.members || []).length}`),
      g.enabled === false ? el('span', { class: 'tag expired' }, '池子已停用') : null));
  }
}

async function saveDistModal() {
  const item = {
    id: distEditing.id || '',
    name: $('gd-name').value.trim(),
    enabled: $('gd-en').value === '1',
    pools: distEditing.pools || [],
  };
  if (!item.name) { toast('给它起个名字（谁在用），日后好停用', false); return; }
  const isNew = !item.id;
  const r = await invoke('action', { payload: { action: isNew ? 'proxy_dist_create' : 'proxy_dist_update', item } });
  if (!r.ok) { toast(r.msg, false); return; }
  if (isNew) {
    // 后端把刚生成的明文放在 msg 里，就地贴出来方便立刻复制（关掉弹窗就再也看不到）
    const key = (r.msg.match(/ac-[0-9a-f]{32}/) || [''])[0];
    const out = $('gd-out');
    clear(out);
    out.hidden = false;
    const code = el('code', { class: 'mono' }, key || r.msg);
    out.append(
      el('div', { class: 'gwkey' }, code, btn('复制', 'sm', async () => {
        try {
          await navigator.clipboard.writeText(code.textContent);
          toast('已复制分发 Key');
        } catch (e) { toast(`复制失败：${e}`, false); }
      }, '复制到剪贴板')),
      el('div', { class: 'stamp' }, '填到客户端的 API Key 位置；接口地址用本机反代地址。之后要再看，在列表上点「显示」。'));
    distEditing.id = '';   // 再点一次「保存」＝再生成一个
    distEditing.name = '';
  }
  toast(isNew ? '已生成新的分发 Key' : '已保存');
  load();
}

/* ---------------- API 与额度：站点 → 多个 key 两级结构 ---------------- */
function kindChip(kind) {
  return chip(kind === 'relay' ? '中转' : kind === 'local' ? '本地' : '官网', kind || 'official');
}
function expChip(k) {
  if (k.days_left == null) return chip('未填', '');
  if (k.days_left < 0) return chip('已过期', 'expired');
  return chip(`${k.days_left} 天`, k.days_left <= 30 ? 'soon' : '');
}
/* 站点额度的一句话：剩 X / 共 Y；只填了一半就只显示那一半 */
function siteQuotaTxt(p) {
  const l = p.left, t = p.total;
  if (l == null && t == null) return '';
  if (l == null) return `共 ${nfmt(t)}`;
  if (t == null) return `剩 ${nfmt(l)}`;
  return `剩 ${nfmt(l)} / 共 ${nfmt(t)}`;
}

/* 整张列表每次都是推倒重建：clear() 一执行，滚动容器的 scrollHeight 当场塌掉，
   scrollTop 被浏览器夹回 0——点开下面那张卡就跳回页顶，轮询一次也跳一次。
   所以重建前记住位置，重建后原样放回去（侧栏 renderNav 同一套做法）。 */
function renderKeys(d) {
  const box = $('keysites').closest('.pagebody');
  const keep = box ? box.scrollTop : 0;
  renderKeyList(d);
  if (box) box.scrollTop = keep;
}

function renderKeyList(d) {
  const host = $('keysites');
  clear(host);
  let sites = d.keys || [];
  const totalKeys = sites.reduce((s, p) => s + (p.keys?.length || 0), 0);
  $('keys-count').textContent = sites.length
    ? `${sites.length} 个站点 · ${totalKeys} 个 key`
    : '';
  if (S.keysSoonOnly) {
    sites = sites.map((p) => ({
      ...p,
      keys: (p.keys || []).filter((k) => k.days_left != null && k.days_left <= 30),
    })).filter((p) => p.keys.length);
  }
  if (!sites.length) {
    host.appendChild(el('div', { class: 'empty' },
      S.keysSoonOnly ? '没有 30 天内过期的 key'
        : '还没有站点。用上面「快速登记」把那段信息贴进来（截图也行），站点和 key 一起建好；想一条条填就点右上「＋ 添加站点」。'));
    renderKeyNav(d);
    return;
  }

  for (const p of sites) {
    const keys = p.keys || [];
    const box = el('section', {
      class: `panel site${S.keysOpen[p.id] ? ' open' : ''}`,
      id: `site-${p.id}`,          // 左菜单点站点名要能滚到它
    });

    const flip = () => { S.keysOpen[p.id] = !S.keysOpen[p.id]; renderKeys(S.data); };
    const toggle = el('button', {
      class: 'sitetoggle', type: 'button',
      title: S.keysOpen[p.id] ? '收起' : '展开',
      aria: { expanded: String(!!S.keysOpen[p.id]) },
    }, el('span', { class: 'caret' }, S.keysOpen[p.id] ? '▾' : '▸'));
    // 只有这个箭头钮管展开/收起。整行可点曾经害得箭头点了没反应（被 head 的
    // closest('button') 拦掉），现在反过来：点名字要能选中复制，所以别处一律不接管。
    toggle.addEventListener('click', (e) => { e.stopPropagation(); flip(); });

    // 站点头：名字、类型、key 数、额度（额度挂在站点上，不是每个 key 一份）、操作
    const head = el('div', { class: 'sitehead' },
      toggle,
      el('div', { class: 'sitename' },
        el('b', {}, p.name || '未命名站点'),
        kindChip(p.kind),
        el('span', { class: 'sitecount', title: '该站点下登记的 key 数量' }, `${keys.length} 个 key`),
        (p.left != null || p.total != null)
          ? el('span', { class: 'sitecount', title: '站点额度（剩余 / 总额），在「改站点」里填' }, siteQuotaTxt(p))
          : null,
        p.homepage ? el('a', { class: 'sitehome', href: p.homepage, target: '_blank', rel: 'noreferrer', title: p.homepage }, '主页') : null),
      el('span', { class: 'sp' }),
      btn('＋ 加 key', 'sm', () => openKeyModal(p, null),
        '在这个站点下再加一个 key：用途标签、余额与过期日各自一份；接入点用站点上填好的'),
      btn('改站点', 'sm', () => openSiteModal(p), '改站点名、类型、接入点（Base URL）与可用模型'),
      btn('删站点', 'danger sm', async () => {
        if (await askConfirm(
          `删除站点「${p.name}」连同它下面 ${keys.length} 个 key。\n删除后无法再查看或修改，正在用这些 key 的程序会失败。`,
          { danger: true, okText: '删除站点', title: '删除站点' })) {
          run('key_delete', { site: p.id });
        }
      }, '删除整个站点（含它下面所有 key）'));
    box.appendChild(head);

    // 站点下的：接入点 + 模型清单 + key 表
    if (S.keysOpen[p.id]) {
      const info = siteInfo(p);
      if (info) box.appendChild(info);
      const tb = el('tbody');
      for (const k of keys) {
        tb.appendChild(keyRow(p, k));
      }
      box.appendChild(el('div', { class: 'tablewrap' },
        el('table', { class: 'tbl keys' },
          el('thead', {}, el('tr', {},
            el('th', { class: 'k-label' }, '用途'),
            el('th', { class: 'k-key' }, 'Key'),
            el('th', { class: 'k-exp' }, '过期'),
            el('th', { class: 'k-st' }, '测试'),
            el('th', { class: 'k-act' }, '操作'))),
          keys.length ? tb : null)));
      if (!keys.length) {
        box.appendChild(el('div', { class: 'empty' },
          '这个站点还没有 key。点右上「＋ 加 key」登记——同一个站可以有多个 key，各自用于不同地方。'));
      }
    }
    host.appendChild(box);
  }
  renderKeyNav(d);   // 左菜单的站点计数跟着列表一起更新
}

/* 站点的主要信息：接入点 + 可用模型。这两样挂在站点上（一个站常有
   OpenAI / Anthropic / DashScope 三种口，模型清单是全站共用的），
   key 表里就不再各占一列，免得六列挤成横向滚动。
   两样都没有时整块不渲染——空提示把卡片撑成一坨灰底，比没有还碍事。 */
function siteInfo(p) {
  const bases = (p.base_urls || []).filter((b) => String(b.url || '').trim());
  const models = String(p.models_list || '').split(/[,，\s]+/).filter(Boolean);
  if (!bases.length && !models.length) return null;

  const rows = el('div', { class: 'sibases' });
  for (const b of bases) {
    const fmt = b.format || 'openai';
    const label = (BASE_FMT.find((x) => x[0] === fmt) || [, fmt])[1] || fmt;
    rows.appendChild(el('div', { class: 'sirow' },
      chip(label, 'fmt'),
      el('span', { class: 'url', title: b.url }, b.url),
      btn('复制', 'plain sm', () => copyText(b.url), '复制这个接入点'),
      btn('拉模型', 'plain sm', () => pullSiteModels(p, b),
        `向 ${b.url}/models 发一次请求，把拉到的模型并进站点的「可用模型」`)));
  }

  const chips = el('div', { class: 'sichips' });
  for (const m of models.slice(0, 40)) {
    const c = el('button', { type: 'button', class: 'chip', title: '点击复制模型名' }, m);
    c.addEventListener('click', () => copyText(m));
    chips.appendChild(c);
  }
  if (models.length > 40) chips.appendChild(el('span', { class: 'muted' }, `+${models.length - 40}`));

  return el('div', { class: 'siteinfo' },
    bases.length ? rows : null,
    models.length
      ? el('div', { class: 'sigroup' }, el('span', { class: 'silab' }, `模型 ${models.length}`), chips)
      : null);
}

async function pullSiteModels(site, b) {
  toast(`正在向 ${b.url} 要模型清单…`, true);
  const r = await invoke('action', { payload: { action: 'site_models', site: site.id, base: b.url } });
  toast(r.msg, r.ok);
  if (r.ok) load();
}

/* API 页左菜单：站点一多，光靠滚动找太累 */
function renderKeyNav(d) {
  const nav = $('key-nav');
  if (!nav) return;
  clear(nav);
  const sites = d.keys || [];
  const jump = (id) => {
    const t = id ? $(id) : null;
    if (t) t.scrollIntoView({ block: 'start' });
    else {
      const box = nav.closest('.pagebody');
      if (box) box.scrollTop = 0;
    }
  };
  const item = (label, on, fn, title) => {
    const b = el('button', { type: 'button', title }, label);
    if (on) b.setAttribute('aria-current', 'true');
    b.addEventListener('click', fn);
    return el('li', {}, b);
  };
  nav.appendChild(item('快速登记', false, () => {
    jump('key-intake');
    const box = $('k-intake');
    if (box) box.focus();
  }, '跳到页顶的登记框'));
  nav.appendChild(item(`全部站点 ${sites.length}`, !S.keyNavSite, () => {
    S.keyNavSite = '';
    jump(null);
    renderKeyNav(d);
  }, '收起定位，回到列表开头'));
  for (const p of sites) {
    const n = (p.keys || []).length;
    nav.appendChild(item(`${p.name} ${n}`, S.keyNavSite === p.id, () => {
      S.keyNavSite = p.id;
      S.keysOpen[p.id] = true;
      renderKeys(S.data);        // 它会顺手把这份菜单重画一遍
      jump(`site-${p.id}`);
    }, `展开并跳到「${p.name}」`));
  }
}

/* key 行只留五列：接入点和模型清单挂在站点上，额度也挂在站点上（见 siteQuotaTxt）。
   操作列照 LongCat 的 API Keys 那样用图标，悬停 tooltip 说明；顺序按她点名的：
   测试 / 编辑 / 复制 / 接入 / 删除 */
function keyRow(site, k) {
  return el('tr', {},
    el('td', { class: 't', title: k.note || k.label || '' }, k.label || el('span', { class: 'muted' }, '未命名')),
    el('td', { class: 'k-key', title: '点「复制」取全文' },
      el('span', { class: 'mono' }, k.masked || '—'),
      k.use_in_proxy ? chip('进反代', 'swap') : null,
      k.base_url ? chip('自带接入点', '') : null),
    el('td', {}, expChip(k)),
    el('td', {}, k.last_status ? chip(k.last_status === 'ok' ? '可用' : '失败', k.last_status) : chip('未测', '')),
    el('td', { class: 'acts' },
      iconBtn('test', () => run('key_test', { site: site.id, id: k.id }),
        '测试：向站点的接入点发一次 GET /models，验证这把 Key 还能不能用'),
      iconBtn('edit', () => openKeyModal(site, k),
        '编辑：改用途、Key 原文、接入点覆盖与过期日'),
      iconBtn('copy', () => copyKey(site.id, k.id, k.label || site.name),
        '复制：把完整 Key 复制到剪贴板（列表里始终打码）'),
      iconBtn('plug', () => openInjectModal(site, k),
        '接入：把这条 key 写进 Qoder / WorkBuddy 的自定义模型配置，让那个程序直接能用（写入前自动备份，可逐条撤掉）'),
      iconBtn('trash', async () => {
        if (await askConfirm(
          `删除这条 key 后，正在用它发请求的程序会失败，也无法再查看或修改这条 key。\n同站点其它 key 不受影响。`,
          { danger: true, okText: '删除', title: `删除 API Key${k.label ? ` · ${k.label}` : ''}` })) {
          run('key_delete_key', { site: site.id, id: k.id });
        }
      }, '删除：删掉这条 key，同站点其它 key 不受影响')));
}

/* ---------------- 接入 Agent：把台账里这条 key 写进程序的自定义模型配置 ----------------
   动的是她正在用的程序的配置文件，所以三道闸：程序在跑就不给勾、写之前整份备份、
   只按台账记下的条目 id 撤自己写的那几条（不去猜哪条像驾驶舱加的）。 */
const inj = { site: null, key: null, list: [], sel: new Set(), busy: true, msg: '' };
async function openInjectModal(site, k) {
  inj.site = site; inj.key = k; inj.list = []; inj.busy = true; inj.msg = '';
  inj.sel = new Set();
  $('inj-title').textContent = `接入到哪些程序 · ${site.name}${k.label ? ` / ${k.label}` : ''}`;
  clear($('inj-list'));
  $('injmodal').hidden = false;
  renderInject();
  const r = await invoke('action', { payload: { action: 'inject_targets', site: site.id, id: k.id } });
  inj.busy = false;
  if (!r.ok) { inj.msg = r.msg; toast(r.msg, false); renderInject(); return; }
  let d = {};
  try { d = JSON.parse(r.msg); } catch { /* 后端没给合法 JSON 就按空处理 */ }
  inj.list = d.targets || [];
  const n = String(k.models_list || '').split(/[,，\s]+/).filter(Boolean).length;
  inj.msg = d.vault_locked
    ? '保管箱锁着，拿不到 Key 原文：先到 设置 → 凭据安全 解锁再回来。'
    : `会把这条 key 的 ${n || '（还没勾模型清单，0）'} 个模型写进你勾选的程序。写之前原文件整份备份，之后能逐条撤掉。`;
  renderInject();
}
function renderInject() {
  const box = $('inj-list');
  clear(box);
  const sub = $('inj-sub');
  sub.textContent = inj.busy ? '正在看本机哪些程序能接…' : (inj.msg || '');
  sub.classList.toggle('bad', !inj.busy && !inj.list.length);
  if (inj.busy || !inj.list.length) return;
  for (const t of inj.list) {
    const on = inj.sel.has(t.app);
    const cells = [];
    const cb = el('input', { type: 'checkbox' });
    cb.checked = on;
    cb.disabled = !t.config_exists || t.running;
    cb.addEventListener('change', () => {
      if (cb.checked) inj.sel.add(t.app); else inj.sel.delete(t.app);
      renderInject();
    });
    cells.push(el('td', { class: 't' }, cb));
    cells.push(el('td', { class: 't' },
      el('b', {}, t.label),
      el('div', { class: 'stamp', title: t.file }, t.file)));
    cells.push(el('td', {},
      t.running ? chip('在运行·先关掉', 'expired')
        : !t.config_exists ? chip('还没有配置文件', '')
        : t.injected.length ? chip(`已写入 ${t.injected.length} 条`, 'swap')
        : chip('可写入', 'ok')));
    cells.push(el('td', {}, el('span', { class: 'muted', title: t.note }, t.note)));
    cells.push(el('td', { class: 'acts' }, t.injected.length
      ? btn('撤掉', 'danger sm', async () => {
        if (!await askConfirm(`撤掉驾驶舱往「${t.label}」写过的 ${t.injected.length} 条模型？\n你自己手工加的那些不动。`,
          { danger: true, okText: '撤掉' })) return;
        const r = await invoke('action', { payload: { action: 'inject_revert', site: inj.site.id, id: inj.key.id, app: t.app } });
        toast(r.msg, r.ok);
        if (r.ok) openInjectModal(inj.site, inj.key);
      }, '只删驾驶舱记过账的那几条')
      : null));
    box.appendChild(el('table', { class: 'tbl' }, el('tbody', {}, el('tr', {}, ...cells))));
  }
}
async function injectApply() {
  const apps = [...inj.sel];
  if (!apps.length) { toast('先勾一个程序', false); return; }
  const go = $('inj-go');
  go.disabled = true;
  const r = await invoke('action', { payload: { action: 'inject_apply', site: inj.site.id, id: inj.key.id, apps } });
  go.disabled = false;
  toast(r.msg, r.ok);
  if (r.ok) openInjectModal(inj.site, inj.key);
  load();
}

async function copyKey(siteId, keyId, name) {
  const full = await invoke('get_key_plain', { site: siteId, id: keyId });
  if (!full) { toast('没有读到 Key 原文', false); return; }
  try {
    await navigator.clipboard.writeText(full);
    toast(`已复制「${name}」的 Key 到剪贴板`);
  } catch (e) {
    toast(`复制失败：${e}`, false);
  }
}

/* ---------------- key 查重 & 批量删除 ----------------
   重复主要来自两处：以前识别慢、连点会跑出好几份（现在后端挡住了，历史重复还在）；
   同一段信息也常被先后贴进两个站。查重把同一条 key 原文的所有条目摆到一起，
   每组默认保留一条、其余勾好，确认就删；批量删则是全台账按站摆开自己勾。
   两种模式共用这个弹窗和同一把「删除所选」。 */
const bd = { mode: 'dedup', groups: [], sel: new Set() };

async function openKeyBatch(mode) {
  bd.mode = mode;
  bd.sel = new Set();
  bd.groups = [];
  $('bd-title').textContent = mode === 'dedup' ? 'key 查重 · 保留一条删其余' : '批量删除 key';
  $('bd-sub').textContent = mode === 'dedup' ? '正在按 key 原文扫台账…' : '';
  $('bd-count').textContent = '';
  $('bd-go').disabled = true;
  clear($('bd-list'));
  $('bdmodal').hidden = false;
  if (mode === 'dedup') {
    const r = await invoke('action', { payload: { action: 'key_dedup_scan' } });
    if (!r.ok) { $('bd-sub').textContent = r.msg; return; }
    bd.groups = r.groups || [];
    $('bd-sub').textContent = bd.groups.length
      ? (r.locked
        ? '保管箱锁着，这次按打码串比对（头 6 尾 4 撞车概率极低）。每组默认保留第一条，勾着的确认后删除。'
        : '每组是同一条 key 原文的多条登记。每组默认保留第一条，勾着的确认后删除——先想清楚哪条还在用。')
      : '没有重复：每条 key 原文在台账里只出现一次。';
  } else {
    bd.groups = (S.data.keys || [])
      .map((p) => ({
        masked: '',
        site_name: p.name,
        count: (p.keys || []).length,
        entries: (p.keys || []).map((k) => ({
          site: p.id, site_name: p.name, id: k.id, label: k.label, masked: k.masked,
          left: k.left, days_left: k.days_left, last_status: k.last_status,
          use_in_proxy: k.use_in_proxy,
        })),
      }))
      .filter((g) => g.count);
    $('bd-sub').textContent = '按站点列出全部 key，勾上哪些删哪些；删单个还是列表行里的「删除」更顺手。';
  }
  renderBd();
}

function renderBd() {
  const box = $('bd-list');
  clear(box);
  for (const g of bd.groups) {
    box.appendChild(el('div', { class: 'bdgroup' },
      el('div', { class: 'bdghead' },
        g.masked ? el('span', { class: 'mono' }, g.masked) : el('b', {}, g.site_name),
        el('span', { class: 'kv' }, g.masked ? `${g.count} 条重复` : `${g.count} 个 key`))));
    g.entries.forEach((k, i) => {
      const sig = `${k.site}|${k.id}`;
      const keepHere = bd.mode === 'dedup' && i === 0;
      const cb = el('input', { type: 'checkbox' });
      cb.checked = bd.sel.has(sig);
      cb.disabled = keepHere;
      cb.addEventListener('change', () => {
        if (cb.checked) bd.sel.add(sig); else bd.sel.delete(sig);
        $('bd-go').disabled = !bd.sel.size;
        $('bd-count').textContent = bd.sel.size ? `已勾选 ${bd.sel.size} 条，删除后无法恢复` : '';
      });
      box.appendChild(el('label', { class: 'bdrow' }, cb,
        keepHere ? chip('保留', 'ok') : null,
        el('b', {}, k.label || '未命名'),
        el('span', { class: 'muted' }, k.site_name),
        k.masked ? el('span', { class: 'mono' }, k.masked) : null,
        k.last_status === 'ok' ? chip('可用', 'ok')
          : k.last_status === 'fail' ? chip('失败', 'fail') : null,
        k.use_in_proxy ? chip('进反代', 'swap') : null,
        k.left != null ? el('span', { class: 'kv' }, `剩 ${nfmt(Number(k.left))}`) : null,
        k.days_left != null ? el('span', { class: 'kv' }, `${k.days_left} 天后过期`) : null));
    });
  }
  if (!bd.groups.length) {
    box.appendChild(el('div', { class: 'empty' },
      bd.mode === 'dedup' ? '台账里没有重复的 key。' : '台账里还没有 key。'));
  }
}

async function bdDelete() {
  if (!bd.sel.size) return;
  const ids = [...bd.sel].map((s) => {
    const [site, id] = s.split('|');
    return { site, id };
  });
  if (!await askConfirm(
    `删除勾选的 ${ids.length} 条 key。\n正在用这些 key 的程序会立刻失败，删除后无法恢复。`,
    { danger: true, okText: '删除', title: '批量删除 key' })) return;
  const r = await invoke('action', { payload: { action: 'key_delete_batch', ids } });
  toast(r.msg, r.ok);
  if (r.ok) {
    $('bdmodal').hidden = true;
    await load();
    renderKeys(S.data);
  } else {
    openKeyBatch(bd.mode);   // 勾选里有已经不存在的，重扫一遍
  }
}

/* ---------------- 账号内部设置：一项一行，行下小字解释含义 ----------------
   分组顺序按一个人接手陌生账号时的提问顺序：这是谁的号 → 还剩多少额度 →
   怎么开它/换它 → 数据放在哪 → 接口令牌 → 收尾才是删号。 */
/* 账号设置的左菜单。按标准菜单的形式定：前五项是**名词短语**（这块在讲什么），
   最后一项是不可撤销的动作，单独一档、红字、前面有分隔线——菜单里"改设置"和
   "删东西"永远不该长得一样。每一项底下的内容也按同一节奏排：小标题 → 说明 → 一行一项。 */
const IN_SECTIONS = [
  ['info', '概览'],
  ['quota', '额度与用量'],
  ['run', '运行与账号切换'],
  ['data', '目录与数据'],
  ['auth', '凭据与接入'],
  ['danger', '删除账号'],
];

async function openDetail(app, name, sec = 'info') {
  S.detail = { app, name, sec, dirty: false, meta: {}, size: null, scroll: 0 };
  // 换一张卡就把选中的天清掉：上一张卡选中的日期在这张卡的序列里可能根本没有，
  // 留着会出现"高亮在热力图上、数值行却写着另一天"
  S.trendSel = '';
  $('imodal').hidden = false;
  renderDetail();
  try { S.detail.meta = await invoke('get_meta', { app, name }) || {}; }
  catch { /* 读不到就按空配置显示 */ }
  if (S.detail && !S.detail.dirty) renderDetail();
}
async function closeDetail() {
  if (S.detail?.dirty
    && !await askConfirm('有未保存的改动。\n\n放弃并关闭？', { danger: true, okText: '放弃改动' })) return;
  S.detail = null;
  $('imodal').hidden = true;
}

/* ---------------- 添加本机程序 ---------------- */
function openAddApp() {
  $('am-exe').value = '';
  $('am-name').value = '';
  $('amodal').hidden = false;
  $('am-exe').focus();
}
async function doAddApp() {
  const exe = $('am-exe').value.replace(/^"|"$/g, '').trim();
  if (!exe) { toast('先把程序路径贴进来', false); $('am-exe').focus(); return; }
  const r = await invoke('add_app', { exe, name: $('am-name').value.trim() || null });
  toast(r.msg, r.ok);
  if (r.ok) { $('amodal').hidden = true; load(); }
}

function dField(key, value, attrs = {}) {
  const f = S.detail.f;
  const v = key in f ? f[key] : (value ?? '');
  const inp = el('input', { value: v, ...attrs });
  inp.addEventListener('input', () => { f[key] = inp.value; S.detail.dirty = true; });
  f[key] = v;
  return inp;
}

/* 选目录按钮：路径手打容易错，弹一个和资源管理器同款的对话框回填 */
function browseBtn(get, set, title) {
  return btn('浏览…', 'sm', async () => {
    const r = await invoke('pick_folder', { current: get() || null });
    if (r && r.ok && r.path) set(r.path);
    else if (r && r.msg && r.msg !== '已取消') toast(r.msg, false);
  }, title || '选一个文件夹，不用手打路径');
}

function renderDetail() {
  const d = S.detail;
  const { app, name, meta } = d;
  const i = S.data.instances.find((x) => x.app === app && x.name === name);
  if (!i) { closeDetailNow(); return; }
  const a = S.data.apps.find((x) => x.id === app) || {};
  d.f = d.f || {};
  const q = i.quota;

  $('im-title').textContent = `${i.app_name} / ${name}`;
  $('im-sub').textContent =
    `${kindLabel(i.kind)}`
    + ` · 建于 ${i.created} · ${i.running ? `${i.procs} 个进程在跑` : '未运行'}`
    + (a.shared_profile ? (i.logged_in ? ' · 本机登录中' : '') : (i.swapped ? ' · 当前默认账号' : ''));

  /* ---- 各分组的行 ---- */
  const secs = {};
  // 组内小标题：把"配置 / 数值 / 动作"分开，别一路平铺到分不清主次
  const sub = (t, note) => [el('h3', { class: 'subsec' }, t),
    note ? el('p', { class: 'subsec-desc' }, note) : null];

  const shared = !!a.shared_profile;
  const launchDesc = i.running
    ? (a.isolatable
      ? '只结束这个账号自己的窗口，不动同程序的其它实例；未保存的内容会被强制中断'
      : '该程序不能多开，这会结束整个程序的窗口；未保存的内容会被强制中断')
    : (a.isolatable
      ? '用该账号独立的数据目录开一个新窗口，可与其他账号同时运行；首次打开需在窗口里登录'
      : shared
        ? (i.logged_in
          ? `这份档案现在登的就是「${name}」，打开就是它`
          : `打开程序，在里面退出登录、再登「${name}」。共用一份档案的产品切号就在客户端里做：不用关程序，也不动任何文件`)
        : '直接打开主程序（走默认档案）。该程序会把第二次启动转交给已开窗口，所以唤出来的可能是当前默认账号；要指定这个账号请先换号');
  // 多开能力决定下面两个按钮走哪条路，所以放在最前面当上下文，别混在动作里
  const runRows = [
    ...sub('多开能力', '这个程序能不能同时开多个账号的窗口——它决定了下面「启动」走哪条路。'
      + '判定是内核在这台机器上实测出来的，不是你填的。'),
    row('多开能力', a.isolatable
    ? '内核实测过：这个程序接受独立数据目录，可以和别的账号同屏开窗口'
    : shared
      ? '实测结论：第二次启动会被已开窗口接管，只能一次一个账号。它名下几个账号还共用同一份档案，'
        + '所以切换账号是在程序里退出登录再登录，驾驶舱不搬文件、也不会为了换号去结束你的进程。'
        + '判定错了可在 设置 → 高级 → 重置多开判定 重来'
      : '实测结论：第二次启动会被已开窗口接管，所以只能一次一个账号，换号靠把默认档案指向账号目录。'
        + '判定错了可在 设置 → 高级 → 重置多开判定 重来',
    chip(a.isolatable ? '可多开' : (shared ? '共用一份档案' : (a.kind === 'cli' ? '终端' : '仅可换号')), a.isolatable ? 'official' : 'soon'))];
  runRows.push(...sub('启动与结束', i.running
    ? '现在有窗口在跑，这一行是把它结束掉。'
    : '这一行是把这个账号打开——共用一份档案的产品，打开的就是程序本身。'));
  runRows.push(row(i.running ? '结束运行' : (a.isolatable ? '启动这个账号' : '打开主程序'), launchDesc,
    btn(i.running ? '■ 结束' : '▶ 启动', i.running ? 'danger sm' : 'primary sm',
      () => run(i.running ? 'kill' : (a.isolatable ? 'launch' : 'launch_app'), { app, name }))));
  if (shared) {
    const pack = i.auth_pack;
    runRows.push(...sub('换到这个账号 / 换回去', '一次只登一个号的产品，切号就是这一件事。'));
    runRows.push(row('切换账号',
      i.logged_in
        ? `这份档案现在登的就是「${name}」，不用切。`
        : pack
          ? `${a.name} 存过这个号的登录态（收在 ${stampTime(pack.at)}）。点「切换到此账号」依次是：`
            + '收好当前号的登录态 → 结束客户端 → 把这份写回档案位置 → 重新打开。'
            + '客户端只在启动时读这份文件，所以这一次重启是必要的；换完这张卡会自己标成「本机登录中」。'
          : `这个号还没存过登录态。${a.name} 几个账号共用默认位置那一份档案，所以先在程序里退出登录、登它一次——`
            + '登进来驾驶舱会自动收下这份登录态（约 45 秒内），以后来回切都是一键。',
      i.logged_in ? chip('本机登录中', 'official')
        : a.auth_switch && pack
          ? btn('⇄ 切换到此账号', 'primary sm', () => authSwitch(app, name), '写回这个号存过的登录态，并自动重开客户端')
          : btn('启动客户端', 'primary sm', () => openClient(app, name), '只把程序打开，不做任何换号动作')));
  } else if (a.swap_ok) {
    // 标题是"还原"时描述必须讲还原，别一套话术挂在两个相反的动作上
    runRows.push(row(i.swapped ? '还原默认档案' : '设为当前账号（换号）',
      i.swapped
        ? '这个账号现在就是该程序的默认档案（junction 指着它的目录）。还原 = 把默认档案换回你原来那份；这个账号目录本身保留，随时能再切回来。需先关掉该程序窗口'
        : '把该程序的默认档案目录用 junction 指向这个账号目录，零额外空间；之后从开始菜单/任务栏打开都是这个账号。需先关掉该程序窗口，随时可「还原默认」退回',
      i.swapped
        ? btn('还原默认', 'danger sm', () => run('swap_clear', { app }), '换回你原来的账号目录')
        : btn('设为当前', 'primary sm', () => swapSet(app, name), '切换到这个账号')));
  }
  // 本地会话用量是"用了多少"，归到额度那一组去
  const usageRow = i.kind === 'cli'
    ? row('本地会话用量', '从该账号的本地会话日志汇总真实 Token 用量（目前支持 Codex），不依赖官方配额接口；后台每轮也会自动刷新',
      el('div', { class: 'ctlrow' },
        el('span', { class: 'kv' }, i.usage
          ? `入 ${fmtTok(i.usage.input)} / 出 ${fmtTok(i.usage.output)} / 共 ${fmtTok(i.usage.total)}`
          : '未统计'),
        btn('刷新用量', 'sm', () => run('fetch_usage', { app, name }))))
    : null;
  secs.info = () => secPanel('概览',
    ...sub('这是哪个号', '这一组只说"它是谁"：属于哪个程序、在驾驶舱里叫什么名字。'),
    row('所属程序', '「建于」是这张卡在驾驶舱里登记的日子，不是这个产品的安装时间',
      el('span', { class: 'kv' }, `${i.app_name} · ${kindLabel(i.kind)} · 建于 ${i.created}`)),
    row('账号名', '列表、顶栏临期提醒、备份文件名都按这个名字认人。数据目录会跟着一起改名，但程序正在跑、或这个账号正被挂成默认档案时目录挪不动 —— 那种情况先只改名字，等关掉程序再改一次名字就同步了',
      btn('改名', 'sm', () => renameAccount(app, name))),
    ...sub('登录与档案', '这个号现在登没登、这份档案从哪来——换号、删号之前先看这两行。'),
    row('登录状态', i.identity && i.identity.state === 'logged'
      ? '已检测到登录痕迹，可以直接用'
      : '还没检测到登录：换号切到空目录会让程序显示未登录甚至黑屏，属正常，登录一次即可',
      el('span', { class: 'kv' }, i.identity ? i.identity.label || '—' : '—')),
    row('本机账号', i.quota.local_account || meta.quota_account
      ? '从这份档案里读出来的登录名，用来核对"卡片上的号"和"客户端里登的号"是不是同一个'
      : '还没从任何数据源读到过登录名',
      el('span', { class: 'kv' }, i.quota.local_account || meta.quota_account || '—')),
    row('档案来源', originText(i) === '沿用本机原有档案'
      ? '这张卡认领的就是这台机器上原本那份档案，驾驶舱没有搬动过它'
      : '这张卡的档案是怎么来的；换号时搬来搬去的都是这个目录',
      el('span', { class: 'kv', title: i.seeded_from || '' }, originText(i) || '未登记')));

  secs.run = () => secPanel('运行与换号', ...runRows);

  secs.quota = () => {
    const rows = [
      // ① 额度明细：读进来什么样就照那样画——档名、单位、使用范围都用厂商自己的口径
      ...sub('额度明细 · 逐笔到期',
        '档名和单位照各产品自己的叫法列（通用积分 / 星火专属积分 / 每日签到 / Token 钱包 / 充值余额…），'
        + '带「专属」标记的那档只能花在说明里写的那些东西上。每档再点「查看明细」看逐笔资源包：'
        + '有效期内几笔、已过期几笔、各自什么时候作废（按到期时间从最早的要作废的排起）。'
        + '官方接口没给的数就写"没给"，不当 0 算比例。30 天内变黄、7 天内变红，顶栏「临期 N」汇总，7 天内弹窗提醒一次。'),
      quotaPanel(i),
      ...sub('数据从哪来', '读数只有卡片上那一颗「↻ 刷新额度」：能走官网就走官网（用量、每一个积分包、各自的到期时间、每日消耗一次读全），'
        + '官网读不到才退回本机/服务端汇总，并把缺了哪一层当场说清楚。下面配好任一种，明细就会自己更新；界面上没有手填入口。'),
      ...(a.local_quota || a.web_quota ? [row(a.web_quota ? '这个号的官网会话' : '本机直读',
        a.web_quota
          ? '每个号一份独立会话（存在数据根 webprofile/这个产品/这个号/ 下面），所以点这张卡的刷新读出来的就是这个号的真数，'
            + '跟这台机器的客户端现在登的是谁无关。第一次登录一次，之后不用再登；不碰你的浏览器，也不碰这个产品自己的档案。'
          : '直接解本机客户端自己存的积分（DPAPI + v10/AES-GCM，不联网、不改任何产品文件）。'
            + '第一次读会把 userId 绑定到这个账号，并顺手把账号名改成真实用户名；之后每次读都先校验是不是同一个号，不是就拒绝写入。',
        el('div', { class: 'ctlrow' },
          el('span', { class: 'kv' }, meta.quota_account ? `已绑定 ${meta.quota_account}` : '未绑定'),
          btn('↻ 刷新额度', 'sm', () => refreshQuota(app, name)),
          ...(a.web_quota ? [btn('登录这个号', 'sm', () => webLogin(app, name),
            '只开这个号自己的官网窗口；登一次，之后刷新额度就带着这份会话去读')] : [])))] : []),
      row('额度来源', '留空＝不联网抓。OpenAI 官方 billing 需填「凭据与反代」里的 API Token',
        (() => {
          const s = el('select', {},
            el('option', { value: '' }, '无'),
            el('option', { value: 'openai' }, 'OpenAI 官方 billing'));
          s.value = meta.quota_adapter || '';
          s.addEventListener('change', () => { meta.quota_adapter = s.value; S.detail.dirty = true; });
          return s;
        })()),
      row('额度接口 URL', '填了任何来源，后台都会按 设置 → 额度提醒 里的间隔自动抓，也可手动立即抓',
        dField('quota_url', meta.quota_url, { placeholder: 'https://…/quota', spellcheck: 'false' })),
      row('取值路径', '从返回的 JSON 里取哪个字段，如 data.remaining；留空自动猜常见字段名',
        dField('quota_path', meta.quota_path, { placeholder: 'data.remaining', spellcheck: 'false' })),
      ...sub('立即取数', '平时靠后台按 设置 → 额度提醒 里的间隔自动取，这里只是手动催一次。'),
      row('截图识别', '读不到的那种（官网才有数）：积分页截一张图 → 识别成逐笔积分。这个浮窗开着的时候，截图存进剪贴板后按 Ctrl+V 就能贴，不用先点哪个框；也可以用右边的「选图片…」从文件里挑。模型只照图上的数字读，识别完你要核对再保存',
        pasteHint('quota')),
      row('立即抓一次', '向上面配置的接口查一次剩余额度并回填；没配接口时点了会提示失败',
        btn('抓取', 'sm', () => run('fetch_quota', { app, name }),
          q.quota_url || q.adapter ? '' : '需先选择额度来源或填写额度接口')),
      row('上次抓取', q.fetched_at ? '后台每轮自动抓取后会更新这里' : '还没抓过',
        el('span', { class: 'kv' }, q.fetched_at ? stampTime(q.fetched_at) : '—')),
      usageRow,
    ].filter(Boolean);
    return secPanel('额度与用量', ...rows);
  };

  secs.data = () => secPanel('目录与数据',
    ...sub('路径', '这个账号在哪打开、档案实际存在哪。'),
    (() => {
      const wd = dField('workdir', i.workdir, { placeholder: 'D:\\Projects\\…', spellcheck: 'false' });
      return row('工作目录', '这个账号启动时默认打开的项目文件夹，D 盘也行；只影响程序自己记住的最近目录，不影响登录。账号档案（登录态/设置/缓存）不在这里，见下面「数据目录」',
        el('div', { class: 'ctlrow browse' }, wd,
          browseBtn(() => wd.value, (v) => { wd.value = v; S.detail.f.workdir = v; S.detail.dirty = true; })));
    })(),
    row('数据目录', i.in_place
      ? '这个账号就是本机默认档案本身，数据一直在程序的正常位置，驾驶舱没有搬动它。在资源管理器里打开'
      : '这个账号的独立档案（登录态、设置、缓存）实际存放位置，在资源管理器里打开',
      btn('打开目录', 'sm', () => run('open_dir', { app, name }))),
    ...(i.rehome_to ? [row('把数据搬回原位',
      `早先「接管原有档案」把这个账号的档案搬进了驾驶舱数据根（${meta.data_dir || ''}），`
      + `而 ${i.rehome_to} 才是这个程序自己一直在读的位置。归位＝同盘改名搬回去，不复制；`
      + '搬完它就是原地账号，换号只在同级改名让位。需先关闭该程序窗口',
      btn('归位', 'sm', () => rehomeAccount(app, name)))] : []),
    ...sub('空间', '缓存类都可再生，清掉不会丢登录状态；动手前先关掉该账号窗口。'),
    row('磁盘占用', '统计该账号目录实际占了多少空间',
      el('div', { class: 'ctlrow' },
        el('span', { class: 'kv' }, d.size ?? '未统计'),
        btn('重新统计', 'sm', async () => {
          const r = await run('calc_size', { app, name });
          if (S.detail && r) { S.detail.size = r.msg; renderDetail(); }
        }))),
    row('清理缓存', '删除 Cache / logs / GPU 缓存等可再生文件，登录状态和设置保留',
      btn('清理', 'sm', () => cleanCache(app, name))),
    ...sub('备份与恢复', '备份打包成 zip 存到数据根的 backups/ 下（位置见 设置→存储），会真实占用磁盘（自动跳过缓存）。程序装坏了随时能回滚。'),
    row('立即备份', '备份整个账号目录',
      el('div', { class: 'ctlrow' },
        el('span', { class: 'kv' }, latestBackupLabel(app, name)),
        btn('备份', 'sm', () => run('backup', { app, name })))),
    row('从备份恢复', '用最近一次备份覆盖当前账号目录，备份之后新产生的改动会丢失；需先关闭该账号窗口',
      btn('恢复', 'sm', () => restoreConfirm(app, name))));

  secs.auth = () => secPanel('凭据与反代',
    row('API Token', '该账号的接口令牌：是 OpenAI billing 抓额度的凭据，也是它作为反代上游时替换 Authorization 用的凭据',
      dField('token', meta.token, { type: 'password', placeholder: 'sk-…', autocomplete: 'off' })),
    (() => {
      const inPools = poolsOfInstance(i.app, i.name);
      const state = !i.quota.has_token ? 'none' : (inPools.length ? 'in' : 'idle');
      const desc = state === 'none'
        ? '先在上面填 API Token，它才能在反代里作为上游使用'
        : (state === 'in'
          ? `已归入池子：${inPools.join('、')} —— 要调整去账号库下方「本地反代 → 池子」`
          : '填了 Token 就能用；要让它参与分流，去账号库下方「本地反代」把它勾进一个池子');
      const tag = state === 'none' ? '无 Token' : (state === 'in' ? '已归池' : '待归池');
      return row('反代可用性', desc, chip(tag, state === 'none' ? 'expired' : (state === 'in' ? 'official' : 'soon')));
    })());

  // 「隐藏整个程序」是程序级动作、内容区右上本来就有那颗按钮，不放进"这个账号"的设置里；
  // 改名已挪到「账号信息」，这一组只剩不可回退的那一件事，所以留在最后
  secs.danger = () => secPanel('删除账号',
    row('删除账号', '删除这个账号的档案与整个数据目录（含登录状态），不可恢复。只是不想看到整个程序的话，用内容区右上的「隐藏程序」',
      btn('删除账号', 'danger sm', async () => {
        if (await del(app, name)) closeDetailNow();
      })));

  /* ---- 左侧分组导航 + 当前分组内容 ---- */
  const nav = $('im-nav');
  clear(nav);
  for (const [id, label] of IN_SECTIONS) {
    if (id === 'danger') nav.appendChild(el('li', { class: 'sep', 'aria-hidden': 'true' }));
    const b = el('button', { type: 'button', class: id === 'danger' ? 'danger' : '' }, label);
    b.dataset.sec = id;
    b.setAttribute('aria-current', String(d.sec === id));
    b.addEventListener('click', () => { d.sec = id; d.scroll = 0; renderDetail(); });
    nav.appendChild(el('li', {}, b));
  }
  const body = $('im-body');
  d.scroll = body.scrollTop;
  clear(body);
  body.appendChild((secs[d.sec] || secs.info)());
  body.scrollTop = d.scroll;
}
function closeDetailNow() { S.detail = null; $('imodal').hidden = true; }
function secPanel(title, ...rows) {
  return el('section', { class: 'panel' }, el('h2', {}, title), ...rows);
}

function latestBackupLabel(app, name) {
  const bs = (S.data.backups || []).filter((b) => b.app === app && b.account === name);
  if (!bs.length) return '还没有备份';
  const b = bs[0];
  return `最近 ${b.at} · ${b.mb} MB`;
}

function saveDetail() {
  const { app, name, meta, f } = S.detail;
  // 额度数值不在这里保存：界面上已经没有手填入口，而 save_meta 是"带了字段就覆盖"——
  // 顺手把 credits_left/expires_on 带上去等于每次保存都把本机读到的余额抹掉。
  const payload = {
    app, name,
    workdir: f.workdir ?? '',
    token: f.token ?? '',
    quota_url: f.quota_url ?? '',
    quota_path: f.quota_path ?? '',
    quota_adapter: meta.quota_adapter ?? '',
  };
  run('save_meta', payload);
  // 后端回写后 meta 与界面必须同源，否则下次轮询重绘会把刚填的接口又清空
  Object.assign(meta, {
    token: payload.token, quota_url: payload.quota_url,
    quota_path: payload.quota_path, quota_adapter: payload.quota_adapter,
  });
  S.detail.dirty = false;
}


/* ---------------- 站点编辑弹窗 ---------------- */
let siteEditing = null;
// 接入点挂在站点上：一个站常常同时给 OpenAI / Anthropic / DashScope 三种口
const BASE_FMT = [['openai', 'OpenAI 兼容', 's-b-openai'], ['anthropic', 'Anthropic 兼容', 's-b-anth'], ['other', '其他', 's-b-other']];
function baseOf(list, fmt) {
  const hit = (list || []).find((b) => (b.format || 'openai') === fmt && String(b.url || '').trim());
  return hit ? hit.url : '';
}
function openSiteModal(p) {
  siteEditing = p || { id: '', kind: 'relay' };
  $('sm-title').textContent = p ? `编辑站点 · ${p.name}` : '添加站点';
  $('s-name').value = p?.name || '';
  $('s-kind').value = p?.kind || 'relay';
  const bu = p?.base_urls || [];
  $('s-b-openai').value = baseOf(bu, 'openai');
  $('s-b-anth').value = baseOf(bu, 'anthropic');
  $('s-b-other').value = baseOf(bu, 'other');
  $('s-models').value = p?.models_list || '';
  // 额度挂在站点上：一个站一份总额度/剩余，不是每条 key 各一份
  $('s-total').value = p?.total ?? '';
  $('s-left').value = p?.left ?? '';
  $('s-home').value = p?.homepage || '';
  $('s-note').value = p?.note || '';
  $('smodal').hidden = false;
  $('s-name').focus();
}
function closeSiteModal() { $('smodal').hidden = true; siteEditing = null; }
async function saveSiteModal() {
  const bases = [];
  for (const [fmt, , id] of BASE_FMT) {
    const url = $(id).value.trim();
    if (url) bases.push({ format: fmt, url });
  }
  const item = {
    // 新站点留空 id：后端按归一化站点名 / base_url 自动并入已有站点，不会重复建
    id: siteEditing.id || '',
    name: $('s-name').value.trim(),
    kind: $('s-kind').value,
    base_urls: bases,
    models_list: $('s-models').value.trim(),
    total: $('s-total').value ? Number($('s-total').value) : null,
    left: $('s-left').value ? Number($('s-left').value) : null,
    homepage: $('s-home').value.trim(),
    note: $('s-note').value.trim(),
  };
  if (!item.name) { toast('站点名不能为空', false); return; }
  const r = await invoke('action', { payload: { action: 'key_upsert', item } });
  if (!r.ok) { toast(r.msg, false); return; }
  await load();
  closeSiteModal();
  // 已经有 key 的站（编辑接入点）不用再催她登记一条；新建站才直接进 key 表单
  const site = (S.data.keys || []).find((p) => p.id === r.site)
    || { id: r.site, name: item.name, kind: item.kind, keys: [] };
  S.keysOpen[site.id] = true;
  renderKeys(S.data);
  if (!site.keys.length) openKeyModal(site, null);
}

/* ---------------- key 编辑弹窗（站点下的一个 key） ---------------- */
let keyEditing = null;
function openKeyModal(site, k) {
  keyEditing = { site, key: k || { id: '', use_in_proxy: false } };
  $('km-title').textContent = k
    ? `编辑 key · ${site.name}${k.label ? ` / ${k.label}` : ''}`
    : `为「${site.name}」添加 key`;
  $('k-label').value = k?.label || '';
  // 接入点挂在站点上：这里只放"这条 key 自己要覆盖"的那个，留空表示跟站点一致
  $('k-base').value = k?.base_url || '';
  $('k-base').placeholder = siteHomeBase(site)
    ? `留空＝用站点的 ${siteHomeBase(site)}`
    : '留空＝用站点的接入点（这个站还没填，去「改站点」补）';
  $('k-key').value = '';
  // 过期日是普通文本框：原生 type=date 在中文系统上顶着 "yyyy/mm/日" 的中英杂糅示例，
  // 全站都按 2026-10-20 这种写法走（留空直接填），存的时候统一归一化
  $('k-exp').value = k?.expires_on || '';
  $('k-note').value = k?.note || '';
  $('k-proxy').value = k?.use_in_proxy ? '1' : '0';
  $('kmodal').hidden = false;
  $('k-label').focus();
  if (k) {
    invoke('get_key_plain', { site: site.id, id: k.id })
      .then((v) => { $('k-key').value = v || ''; })
      .catch(() => {});
  }
}
/// 站点默认接入点：挂在站点上的那几个里挑 OpenAI 兼容口，没有再退回任一个；
/// 老数据（站点没记、只有 key 上有 base_url）也照样能用
function siteHomeBase(site) {
  const list = site?.base_urls || [];
  const good = (f) => (list.find((b) => (b.format || 'openai') === f && String(b.url || '').trim()) || {}).url;
  return (good('openai') || good('anthropic') || good('other')
    || (list[0] || {}).url
    || (site?.keys || []).find((k) => k.base_url)?.base_url || '');
}
function closeKeyModal() { $('kmodal').hidden = true; keyEditing = null; }
/* 过期日只认一套写法：2026-10-20（带时间也收，2026-10-20 21:00）。
   斜杠、点、年月日中文分隔一律收进来转成同一种，存盘前统一 */
function normExpDate(s) {
  const t = String(s || '').trim();
  if (!t) return '';
  let m = t.match(/^(\d{4})[-/.年](\d{1,2})[-/.月](\d{1,2})日?$/);
  if (m) return `${m[1]}-${m[2].padStart(2, '0')}-${m[3].padStart(2, '0')}`;
  m = t.match(/^(\d{4})[-/.](\d{1,2})[-/.](\d{1,2})[ T](\d{1,2}):(\d{2})(?::\d{2})?$/);
  if (m) {
    return `${m[1]}-${m[2].padStart(2, '0')}-${m[3].padStart(2, '0')} ${m[4].padStart(2, '0')}:${m[5]}`;
  }
  return t;
}
function collectKeyItem() {
  const base = $('k-base').value.trim();
  const id = keyEditing.key.id || `k${Date.now().toString(36)}${Math.random().toString(36).slice(2, 6)}`;
  const old = keyEditing.key || {};
  // 额度（总额度/剩余）、模型清单、余额接口这些都不在这张表单里了：额度归站点，
  // 老数据里 key 自带的那份原样透传给后端，别在保存时被空值冲掉
  return {
    id,                                   // 空 id = 新建；同 id = 覆盖更新
    label: $('k-label').value.trim(),
    base_url: base,
    key: $('k-key').value.trim(),
    models_list: old.models_list || '',
    total: old.total ?? null,
    left: old.left ?? null,
    expires_on: normExpDate($('k-exp').value),
    note: $('k-note').value.trim(),
    use_in_proxy: $('k-proxy').value === '1',
    quota_adapter: old.quota_adapter || '',
    quota_url: old.quota_url || '',
    quota_path: old.quota_path || '',
  };
}
async function saveKeyModal(again) {
  const item = collectKeyItem();
  // 编辑已有 key 但没改 Key 文本时，后端必须保留原 Key，不能被空串覆盖
  if (!item.id && !item.key) { toast('新 key 至少要填 Key 原文', false); return; }
  if (item.expires_on && !/^\d{4}-\d{2}-\d{2}/.test(item.expires_on)) {
    toast('过期日没认出来：留空，或按 2026-10-20 填', false);
    return;
  }
  const siteId = keyEditing.site.id;
  const r = await run('key_upsert_key', { site: siteId, item });
  if (!r.ok) return;
  if (again !== true) { closeKeyModal(); return; }
  // 一个站常常一次拿到好几个 key（不同用途各一个）：存完清表留在窗里接着填，
  // base_url 也留着——同站的 key 通常只差用途
  await load();
  const fresh = (S.data.keys || []).find((p) => p.id === siteId) || keyEditing.site;
  const base = $('k-base').value.trim();
  openKeyModal(fresh, null);
  if (base) $('k-base').value = base;
}

/* ---------------- API 文本粘贴识别：容忍任意排版 ---------------- */
/* 目标：用户把一整段（可能很乱、含标题/项目符号/多个 base_url/多行）粘进来，
   也要能拆出「站点名 / 多个 key / 每个 key 的 base_url 与模型」。
   策略：按行扫描 + 分组，把每个 key 归到最近的用途标签下。 */
const PASTE_URL_RE = /https?:\/\/[^\s"'`，,、;；）)\]}>]+/i;
const PASTE_KEY_RES = [
  /\bsk-[A-Za-z0-9][A-Za-z0-9\-_]{12,}/g,
  /\b(?:Bearer|Authorization)\s*[:：=]?\s*([A-Za-z0-9][A-Za-z0-9\-_.]{15,})/gi,
  /\b(?:api[_\s-]?key|apikey|key|token|密钥|令牌|访问令牌)\s*[:：=]\s*['"]?([A-Za-z0-9][A-Za-z0-9\-_.]{15,})['"]?/gi,
];
/// 常见用途名：用于「用途标题 → 下面若干 key」的分组识别
const PASTE_KNOWN_LABELS = [
  'Pot', 'TieZ', 'Tiez', 'WorkBuddy', 'QoderCN', 'Qoder CN', 'OpenCode', 'CodeBuddy',
  '沉浸式翻译', '沉浸式', 'Cherry Studio', 'ChatBox', 'Cursor', 'Windsurf', 'Dify',
  'Trae', 'ZCode', 'Codex', 'Claude', 'Gemini', 'Ollama', 'Continue', 'Aider',
];
const PASTE_LABEL_RE = new RegExp(
  `^(?:${PASTE_KNOWN_LABELS.map((s) => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')).join('|')})$`, 'i');
/// 说明性开头行：不是用途标题，也不是站点名
const PASTE_NOISE_RE = /^(api|说明|接口|格式|文档|示例|用法|usage|note|备注|支持|兼容|完全|对话|消息|列表|注明|注意)/i;
/// 站点名行：一行只有这些词的，是站点标题，不是模型、不是用途
function isSiteNameLine(t) {
  return /^(longcat|aliwelink|openai|anthropic|claude|gemini|deepseek|dashscope|百炼|bigmodel|智谱|siliconflow|硅基|moonshot|kimi|ollama|阿里云)\s*(api)?$/i.test(t.trim());
}

function stripMd(s) {
  return String(s || '')
    .replace(/https?:\/\/[^\s"'`，,、;；）)\]}>]+/gi, ' ')
    .replace(/[*#`_~#>|]/g, ' ')
    .replace(/\s+/g, ' ')
    .trim();
}

/// 这行是不是「标题」：短、无 key、无 URL
function isHeading(line) {
  const t = stripMd(line);
  if (!t || t.length > 40) return false;
  if (PASTE_URL_RE.test(line)) return false;
  return true;
}

/// 这行含 key 原文吗
function keyOf(line) {
  for (const re of PASTE_KEY_RES) {
    re.lastIndex = 0;
    const m = line.match(re);
    if (!m) continue;
    const raw = m[1] || m[0];
    if (raw && raw.length >= 16 && !/^https?:/i.test(raw)) return raw.replace(/['",;，。]+$/, '');
  }
  return null;
}

/// 一行里可能出现的模型名：token 必须像真实模型 id（含数字、版本号、路径分隔符之一）
/// 这样 "Longcat" / "Pot" / "OpenCode" 这类标题才不会被误认为是模型
function modelsInLine(line) {
  const t = stripMd(line);
  if (!t || t.length > 80) return '';
  const parts = t.split(/[,，、\s]+/).map((s) => s.trim()).filter(Boolean);
  if (!parts.length) return '';
  const good = parts.filter((s) =>
    /[A-Za-z]/.test(s) && !/[。！？；]/.test(s) && s.length <= 60 &&
    (/\d/.test(s) || /[-_:./]/.test(s)));
  return good.length ? good.join(', ') : '';
}

function parseApiText(raw) {
  const text = String(raw || '');
  const out = { site: null, keys: [] };
  const lines = text.split(/\r?\n/);

  // ---- 类型判断 ----
  let kind = null;
  if (/中转|relay|proxy|中转站|转发站|第三方/i.test(text)) kind = 'relay';
  else if (/本地|local|localhost|127\.0\.0\.1|ollama|lm\s?studio/i.test(text)) kind = 'local';
  else if (/官网|官方|official|openai|anthropic|claude|gemini|dashscope|百炼|bigmodel|智谱|siliconflow|硅基|deepseek|moonshot|kimi|阿里云|longcat/i.test(text)) kind = 'official';

  // ---- 站点名：显式「站点:」优先；否则取第一个标题行 ----
  let siteName = null;
  const nameHit = text.match(/(?:^|\n)\s*(?:站点|平台|服务|名称|name|platform|title|provider)\s*[:：=]\s*([^\n\r]{1,40})/i);
  if (nameHit) {
    siteName = stripMd(nameHit[1]) || null;
  } else {
    // 跳过开头的空行；遇到第一个非空的标题行，且不是说明句/用途名/模型名，当站点名
    for (let i = 0; i < lines.length; i++) {
      const t = stripMd(lines[i]);
      if (!t) continue;                    // 空行跳过
      if (PASTE_URL_RE.test(lines[i]) || keyOf(lines[i])) break; // 撞到 url/key 就还没站点名
      if (isSiteNameLine(t) || (!PASTE_NOISE_RE.test(t) && !PASTE_LABEL_RE.test(t) && !modelsInLine(lines[i]))) {
        siteName = t;
        break;
      }
      if (!isHeading(lines[i])) break;     // 第一个非空非标题行 → 没站点名
    }
  }

  // ---- 全局模型：显式「模型:」行 ----
  let modelsList = '';
  const explicitModels = text.match(/(?:^|\n)\s*(?:可用模型|模型|models?|model\s*list|支持模型)\s*[:：=]\s*([^\n\r]+)/i);
  if (explicitModels) {
    modelsList = explicitModels[1]
      .split(/[,，、\s]+/).map((s) => s.trim().replace(/^[-*•\d.)\]]+/, ''))
      .filter((s) => s && s.length < 60).join(', ');
  }

  // ---- 逐个 key 向上找「用途 / base_url / 模型」 ----
  // 关键：只有「已知用途名」或「紧邻下一行就是 key 的短标题」才认定为用途，
  // 否则 LongCat-2.0 这类模型名会被当用途，导致后面所有 key 都跟错标签。
  const hits = [];
  lines.forEach((line, idx) => {
    const k = keyOf(line);
    if (k) hits.push({ idx, key: k, line });
  });
  const keyIdxSet = new Set(hits.map((h) => h.idx));

  for (const h of hits) {
    // 最常见的写法是「Pot: sk-xxx」同一行：先剥掉 key，看剩下的是不是一个用途标签。
    // 认出来就不再上溯，否则整站的 key 会一起挂到站点名上（实测踩过）。
    const rest = stripMd(h.line.replace(h.key, ''));
    const inline = /^(.{1,24}?)\s*[:：=]\s*$/.exec(rest);
    let label = inline ? inline[1].trim() : '';
    let base = '';
    let models = '';
    // 用「下一个 key 的位置」划定本 key 的归属区间上沿，避免越界串到别家
    const regionTop = h.idx;
    for (let j = h.idx - 1; j >= 0 && j >= h.idx - 8; j--) {
      const l = lines[j];
      if (keyIdxSet.has(j)) break;                       // 撞到别的 key 就停
      if (!stripMd(l)) {
        // 空行 = 块边界：站点名是一整块（站点描述 + 公共 baseurl + 谁用哪个 key），
        // 用途标签是下一块的第一行。上溯撞到空行就停定位，只认空行之后到 key 之间的内容。
        // （例外：key 前紧邻一行连续空行的也容忍，因为有些人会多敲一行换行）
        const prev = j > 0 ? stripMd(lines[j - 1]) : '';
        if (prev) break;
        continue;
      }
      const u = l.match(PASTE_URL_RE);
      if (u && !base) { base = u[0].replace(/[）)\]}>.,;；]+$/, ''); continue; }
      const t = stripMd(l);
      if (isSiteNameLine(t)) {
        // 站点名是硬边界：它上面的内容属于另一个站点，不再上溯。
        // 但如果这一路没找到任何用途标签，站点名本身就是"这把 key 是干什么用的"最好答案
        // （粘进来的常是「Aliwelink API / image-2 / sk-ali-…」这种站点名当标题的写法）。
        if (!label) label = t;
        break;
      }
      if (PASTE_LABEL_RE.test(t) && !label) { label = t; continue; }
      // 先判模型再判用途：'image-2' / 'LongCat-2.0' 这类带数字或连字符的是模型 id，
      // 不是用途标签。早先反过来的顺序会让 sk-ali 的用途被认成 image-2。
      const mdl = modelsInLine(l);
      if (mdl && !models) models = mdl;
      if (isHeading(l) && !PASTE_NOISE_RE.test(t) && !label && !mdl) { label = t; continue; }
    }
    out.keys.push({
      key: h.key,
      label,
      base_url: base,
      models,
      note: stripMd(h.line.replace(h.key, '').replace(PASTE_URL_RE, '')).slice(0, 80),
    });
  }

  // 站点名：如果第一遍没识别到，用「key 上方第一段非 key 的内容」补全一次
  if (!siteName) {
    for (const l of lines) {
      const t = stripMd(l);
      if (!t || keyOf(l) || PASTE_URL_RE.test(l)) break;
      if (isSiteNameLine(t)) { siteName = t; break; }
      // 第一个短标题且不是用途/模型名的，当站点名
      if (isHeading(l) && !PASTE_NOISE_RE.test(t) && !PASTE_LABEL_RE.test(t) && !modelsInLine(l)) {
        siteName = t;
        break;
      }
    }
  }

  // 模型兜底：标题区里「像模型名」的行（LongCat-2.0 / image-2 这类）
  if (!modelsList) {
    for (const l of lines) {
      if (PASTE_URL_RE.test(l) || keyOf(l)) break;
      const t = stripMd(l);
      if (!t || t === siteName || PASTE_LABEL_RE.test(t) || isSiteNameLine(t) || PASTE_NOISE_RE.test(t)) continue;
      const mdl = modelsInLine(l);
      if (mdl) { modelsList = mdl; break; }
    }
  }
  if (modelsList) {
    for (const k of out.keys) if (!k.models) k.models = modelsList;
  }

  // 站点默认 base_url：站点名之后的第一个 URL（通常是 OpenAI 格式主入口）
  let defaultBase = '';
  const urlAll = text.match(PASTE_URL_RE);
  if (urlAll) {
    const nameIdx = siteName ? lines.findIndex((l) => stripMd(l) === siteName) : -1;
    for (let i = Math.max(nameIdx, 0); i < lines.length; i++) {
      const u = lines[i].match(PASTE_URL_RE);
      if (u) { defaultBase = u[0].replace(/[）)\]}>.,;；]+$/, ''); break; }
    }
  }

  // 每个 key 没认出 base_url 时，回退到站点默认 base_url
  for (const k of out.keys) {
    if (!k.base_url && defaultBase) k.base_url = defaultBase;
  }

  // 一段里常同时声明 OpenAI 与 Anthropic 两种接入点（Longcat 就是），全记下来备用，
  // 免得只剩一个、逐 key 选格式时还得回去翻原文
  const bases = [];
  for (const l of lines) {
    const m = l.match(PASTE_URL_RE);
    if (!m) continue;
    const url = m[0].replace(/[）)\]}>.,;；]+$/, '');
    const fmt = /anthropic|claude/i.test(l) ? 'anthropic' : 'openai';
    if (!bases.some((b) => b.url === url)) bases.push({ format: fmt, url });
  }

  out.site = { name: siteName, kind, base_url: defaultBase, models_list: modelsList, bases };
  return out;
}

/// 从整段文本里捞余额 / 总额 / 过期日——「粘贴识别」和「快速登记」共用一套取数口径
function quotaFromText(raw) {
  const t = String(raw || '');
  const num = (re) => {
    const m = t.match(re);
    if (!m) return null;
    const n = parseFloat(m[1].replace(/,/g, ''));
    return Number.isFinite(n) ? n : null;
  };
  const exp = t.match(/(?:过期|到期|expires?|expire|expiry)[^\d]{0,14}(\d{4}[-/.年]\d{1,2}[-/.月]\d{1,2})/i);
  return {
    left: num(/(?:余额|剩余|left|remaining|balance)\s*[:：=]?\s*([\d][\d.,]*)/i),
    total: num(/(?:总额|总量|total|quota)\s*[:：=]?\s*([\d][\d.,]*)/i),
    expires_on: exp ? exp[1].replace(/[./]/g, '-').replace('年', '-').replace('月', '-') : '',
  };
}

const AI_PROMPT_KEYS = '下面是某个 AI 中转站 / API 的信息文本（可能乱序、可能含多个 key）。'
  + '只输出一个 JSON 本体：不要 markdown 代码块围栏，不要任何解释文字。\n'
  + '{"site":{"name":"站点名","base_urls":[{"format":"openai 或 anthropic 或 dashscope 或 other","url":"https://…"}],'
  + '"models_list":"这个站能用的模型，逗号分隔"},'
  + '"keys":[{"label":"这条 key 的用途名","key":"sk-…","base_url":"只有这条 key 走别的地址时才填，否则空字符串","models":""}],'
  + '"left":剩余额度数字或null,"total":总额度数字或null,"expires_on":"YYYY-MM-DD，没有就空字符串"}\n'
  + '规则：key 原样照抄，一个字符都不要改；文本里有几个 key 就输出几个；'
  + '同一个站的几种接入点全部放进 site.base_urls，不要拆到每条 key 上；'
  + '没有的字段留空字符串或 null，不要编造。';

/* ---------------- AI 识别的忙锁与进度 ----------------
   识别慢（十几秒到一两分钟）人又看不见它在干什么，就会连点——以前一点就是
   一轮新的识别，最后弹出一堆重复结果。现在两头一起管：
   后端 ai_ask 同时只放行一次（后来的直接被挡回去），前端在这里把
   「正在干什么、等了多少秒、模型吐到第几个字」常驻在右下角讲给人听。 */
const AI_BUSY = { on: false, t0: 0, timer: null, label: '' };

function aiProgressShow(label) {
  AI_BUSY.on = true;
  AI_BUSY.t0 = Date.now();
  AI_BUSY.label = label;
  let bar = $('ai-progress');
  if (!bar) {
    bar = el('div', { id: 'ai-progress', hidden: true },
      el('span', { class: 'aip-txt' }));
    document.body.appendChild(bar);
  }
  bar.hidden = false;
  clearInterval(AI_BUSY.timer);
  AI_BUSY.timer = setInterval(aiProgressTick, 700);
  aiProgressTick();
}

async function aiProgressTick() {
  if (!AI_BUSY.on) return;
  const bar = $('ai-progress');
  if (!bar) return;
  const sec = Math.round((Date.now() - AI_BUSY.t0) / 1000);
  let tail = '';
  try {
    const r = await invoke('action', { payload: { action: 'ai_progress' } });
    if (r && r.ok && r.text) tail = r.text;
  } catch { /* 后端正忙，这一拍只显示秒数 */ }
  const txt = bar.querySelector('.aip-txt');
  if (txt) txt.textContent = `${AI_BUSY.label} · 已等 ${sec} 秒${tail ? ' · ' + tail : ''}`;
}

function aiProgressHide() {
  AI_BUSY.on = false;
  clearInterval(AI_BUSY.timer);
  AI_BUSY.timer = null;
  const bar = $('ai-progress');
  if (bar) bar.hidden = true;
}

function aiBusyRefuse() {
  toast('上一次识别还没结束——看右下角的进度，等它出结果，不用连点', false);
}

/* ---------------- 快速登记：一段文字或一张截图 → 站点与 key 当场落盘 ----------------
   本机规则先跑（离线、不花 token）；规则连一个 key 带地址都没认出来，才交给
   设置 → AI 识别 那条通道。认出来的东西直接建成站点 + 多条 key 存进台账，
   事后在列表里逐条改——登记不再是"填完一张表才能存一条"。 */
async function intakeRegister(imageDataUrl) {
  const box = $('k-intake');
  const info = $('k-intake-info');
  const go = $('k-intake-go');
  const raw = box.value;
  if (!imageDataUrl && !raw.trim()) { toast('先把 API 信息贴进来，文字或截图都行', false); return; }
  if (AI_BUSY.on) { aiBusyRefuse(); return; }
  go.disabled = true;
  let j = null;
  let who = '本机规则';
  const p = imageDataUrl ? null : parseApiText(raw);
  if (p && ((p.keys || []).length || p.site?.base_url)) {
    j = { site: p.site, keys: p.keys, ...quotaFromText(raw) };
  } else {
    who = 'AI';
    aiProgressShow(imageDataUrl ? 'AI 读截图' : 'AI 读文本');
    info.textContent = imageDataUrl ? '正在让模型读这张截图…（进度看右下角）' : '本机规则没认出 key 或地址，交给模型读…（进度看右下角）';
    let r = null;
    try {
      r = await invoke('action', {
        payload: {
          action: 'ai_ask',
          prompt: imageDataUrl ? AI_PROMPT_KEYS : `${AI_PROMPT_KEYS}\n\n${raw}`,
          image: imageDataUrl || '',
        },
      });
    } finally {
      aiProgressHide();
      go.disabled = false;
    }
    if (!r.ok) { info.textContent = `模型没读成：${r.msg}`; toast(r.msg, false); return; }
    if (!r.json) {
      info.textContent = '模型没给出可解析的 JSON';
      toast(`模型没给出 JSON：\n${String(r.text).slice(0, 300)}`, false);
      return;
    }
    j = r.json;
  }
  const res = await saveIntake(j, who);
  go.disabled = false;
  info.textContent = res.msg;
  if (res.ok) box.value = '';
  toast(res.msg, res.ok);
}

async function saveIntake(j, who) {
  const site = j.site || {};
  const name = String(site.name || '').trim();
  const keys = (Array.isArray(j.keys) ? j.keys : []).filter((k) => k && String(k.key || '').trim());
  if (!name) return { ok: false, msg: '没认出站点名——在「＋ 添加站点」里补一个，别建出错乱的站' };
  if (!keys.length && !site.base_url) return { ok: false, msg: '这段里既没认出 key 也没认出地址，先手工登记吧' };
  // 接入点与模型清单存到站点上：一个站常同时给 OpenAI / Anthropic / DashScope 几种口
  const bases = [];
  const pushBase = (url, fmt) => {
    const u = String(url || '').trim();
    if (u && !bases.some((b) => b.url === u)) bases.push({ format: fmt || (/anthropic|claude/i.test(u) ? 'anthropic' : 'openai'), url: u });
  };
  for (const b of (site.bases || site.base_urls || [])) pushBase(b.url, b.format === 'anthropic' ? 'anthropic' : 'openai');
  pushBase(site.base_url);
  for (const k of keys) pushBase(k.base_url);
  const modelSet = [];
  const pushModels = (s) => {
    for (const m of String(s || '').split(/[,，\s]+/)) {
      if (m && !modelSet.includes(m)) modelSet.push(m);
    }
  };
  pushModels(site.models_list);
  for (const k of keys) pushModels(k.models || k.models_list);
  // 额度挂站点：文本里认出的 余额/总额 记到站点一份（没认出就不带这个字段，
  // 别把站点上已有的额度冲掉），key 上不再每条一份
  const siteItem = {
    action: 'key_upsert',
    item: {
      id: '', name, kind: site.kind || 'relay',
      base_urls: bases, models_list: modelSet.join(', '), homepage: '', note: '',
    },
  };
  if (j.left != null) siteItem.item.left = j.left;
  if (j.total != null) siteItem.item.total = j.total;
  const s = await invoke('action', { payload: siteItem });
  if (!s.ok) return { ok: false, msg: `站点没建出来：${s.msg}` };
  const siteBase = (bases.find((b) => b.format === 'openai') || bases[0] || {}).url || '';
  let done = 0;
  let firstFail = '';
  for (const k of keys) {
    const own = String(k.base_url || '').trim();
    const item = {
      id: '',
      label: String(k.label || '').trim(),
      base_url: own && own !== siteBase ? own : '',   // 跟站点一样就不重复存，留空表示"用站点的"
      key: String(k.key).trim(),
      models_list: '',                                 // 模型清单归站点，key 上不再各存一份
      expires_on: String(j.expires_on || '').trim(),
    };
    const r = await invoke('action', { payload: { action: 'key_upsert_key', site: s.site, item } });
    if (r.ok) done += 1; else if (!firstFail) firstFail = r.msg;
  }
  await load();
  S.keysOpen[s.site] = true;
  renderKeys(S.data);
  const miss = keys.length - done;
  return {
    ok: true,
    msg: `${who}认出 ${keys.length} 个 key、${bases.length} 个接入点、${modelSet.length} 个模型：`
      + `站点「${name}」已存好` + (miss ? `，${miss} 个 key 没落盘（${firstFail}）` : '，逐条核对后可用'),
  };
}

/* ---------------- 新建账号向导 ---------------- */
// 每个 Agent 支持哪些登录方式 —— **只摆真正接通了的**，不摆点下去什么都不发生的选项。
const METHODS = {
  official: {
    t: '官方登录',
    d: '打开官方客户端，你在窗口里完成登录；Cockpit 一直盯着，检测到登录数据就自动往下走。',
  },
  import: {
    t: '导入已有数据',
    d: '把这个程序本机已有的档案接管或复制进来：登录状态、对话、记忆一起带过来，通常连登录都不用重做。',
  },
  apikey: {
    t: 'API Key',
    d: '填上游地址与 Key：账号建好，key 同时记进台账（可再加进反代池，给别的客户端当 API 用）。',
  },
};
// 还没接通的方式：只用来说明"这条路也在规划里"，不做成能点却没反应的按钮
const METHODS_SOON = [
  { t: 'OAuth 授权', d: '走浏览器授权回调自动取凭据' },
  { t: 'Token / JSON', d: '直接粘贴 token 或凭据 JSON 落盘' },
];
function methodsFor(app) {
  return app.kind === 'cli' ? ['official', 'apikey', 'import'] : ['official', 'import', 'apikey'];
}
// 上游预设：只预填我确信的地址，其它留空让你自己贴（填错了比不填更烦）
const PROVIDERS = [
  { n: 'OpenAI 官方', b: 'https://api.openai.com/v1' },
  { n: 'DeepSeek', b: 'https://api.deepseek.com/v1' },
  { n: 'Moonshot', b: 'https://api.moonshot.cn/v1' },
  { n: '阿里云百炼', b: 'https://dashscope.aliyuncs.com/compatible-mode/v1' },
  { n: '智谱 GLM', b: 'https://open.bigmodel.cn/api/paas/v4' },
  { n: 'SiliconFlow', b: 'https://api.siliconflow.cn/v1' },
  { n: 'OpenRouter', b: 'https://openrouter.ai/api/v1' },
  { n: '自定义', b: '' },
];

// 顺序是照着真实登录过程排的，不是照着程序内部结构排的：
// 先选方式 → 真的去登录 → 认出登的是谁（自动填基本信息）→ 才谈档案怎么带、怎么管 → 读额度 → 最后确认建卡。
const WZ_STEPS = [
  { id: 'method', t: '登录方式' },
  { id: 'login', t: '登录' },
  { id: 'setup', t: '基本信息' },
  { id: 'data', t: '数据来源' },
  { id: 'quota', t: '额度' },
  { id: 'done', t: '确认' },
];
// 登录要跳浏览器官网的那些（其余是在客户端窗口里输手机号/扫码）。
// 只影响这一步怎么跟你解释，不影响顺序——所有 Agent 走同一条流程。
const BROWSER_LOGIN = ['qoder', 'qoder-cn', 'qoder-cn-ide', 'workbuddy', 'workbuddy-intl',
  'minimax', 'traework-cn', 'codex', 'claude'];
function loginHint(app) {
  return BROWSER_LOGIN.includes(app.id)
    ? '点下面的「打开客户端」，在客户端里点登录：它会跳到官网的浏览器页面，你在那儿选一个账号授权，'
      + '回到客户端就是登录成功。驾驶舱一直盯着，认出新的登录账号就自动往下走，基本信息我来填。'
    : '点下面的「打开客户端」，在客户端窗口里完成登录（手机号验证码 / 扫码）。'
      + '驾驶舱一直盯着，认出新的登录账号就自动往下走，基本信息我来填。';
}
function factsLine(s) {
  const f = s.facts || {};
  const bits = [];
  if (f.who) bits.push(`登录账号 ${f.who}`);
  if (f.last_used) bits.push(`最近使用 ${f.last_used}`);
  bits.push(f.files ? `${f.files} 个文件` : '里面是空的');
  if (f.mb) bits.push(`${f.mb} MB`);
  return bits.join(' · ');
}
const ORIGIN_TXT = { adopt: '接着用本机原来那份', copy: '复制一份带来对话与记忆', fresh: '从零开始，空目录' };
function openWizard() {
  const app = S.data.apps.find((a) => a.id === S.cur);
  if (!app) return;
  if (!app.installed) { toast('未找到该程序的可执行文件，无法新建账号', false); return; }
  const existing = S.data.instances.filter((i) => i.app === app.id).length;
  S.wizard = {
    step: 0, app,
    m: methodsFor(app)[0],
    name: `账号${existing + 1}`,
    workdir: S.data.instances.find((i) => i.app === app.id)?.workdir || '',
    created: null,
    mode: app.isolatable ? 'isolate' : 'swap',
    // 'auto' = 由后端按"默认档案 > 已有账号 > 空档"的规则挑；用户点了具体选项就写死
    origin: 'auto',
    sources: null, recommend: '', recommend_from: '', claimed_by: '',
    up: { name: '', base: '', key: '', models: '' },   // API Key 方式的上游信息
    login: null,        // 第 2 步读到的"本机现在登的是谁"
    web: null,          // 第 5 步读到的官网逐资源包额度
    opened: false, nameTouched: false, credits: null,
    plan: 'new', claimer: '',
    poll: null,
  };
  $('wizard').hidden = false;
  $('wz-title').textContent = `添加 ${app.name} 账号`;
  renderWizard();
  loadSeedSources();
}
// 「本机有哪些档案可带过来」要现算体积（可能上 GB）+ 探是谁在用，只在打开向导时点一次
async function loadSeedSources() {
  const w = S.wizard;
  if (!w) return;
  const r = await run('seed_sources', { app: w.app.id });
  if (!r || !r.ok || S.wizard !== w) return;
  w.sources = r.sources || [];
  w.recommend = r.recommend || '';
  w.recommend_from = r.recommend_from || '';
  w.claimed_by = r.default_claimed_by || '';
  renderWizard();
}
// 第 2 步：读"这台机器现在登的是谁"。认出来了就顺手把基本信息填上、自动进下一步。
async function wzReadLogin(auto) {
  const w = S.wizard;
  if (!w) return null;
  const r = await run('machine_login', { app: w.app.id, refresh: true });
  if (S.wizard !== w) return null;
  w.login = r && r.ok !== undefined ? r : null;
  const L = w.login;
  if (L && L.ok && L.account) {
    const dup = S.data.instances.find(
      (i) => i.app === w.app.id && (i.quota?.user_id === L.user_id || i.quota?.local_account === L.account));
    L.card = dup ? dup.name : '';
    // 名字跟着"最新读到的那个号"走：她在客户端里来回切号时，第一次读到的号不算数。
    // 只有她自己动过名称输入框才不再覆盖（nameTouched）。
    if (!w.nameTouched) w.name = L.account;
    if (auto && w.step === 1 && !dup) { w.step = 2; renderWizard(); return L; }
  }
  renderWizard();
  return L;
}
function stopWzPoll() {
  if (S.wizard && S.wizard.poll) { clearInterval(S.wizard.poll); S.wizard.poll = null; }
}
// 打开客户端 + 每 3 秒读一次登录状态，认出账号就停（不再空跑子进程）
async function wzOpenClient() {
  const w = S.wizard;
  if (!w) return;
  const r = await run('launch_app', { app: w.app.id });
  if (!r || !r.ok) { toast(r && r.msg ? r.msg : '打不开客户端', false); return; }
  w.opened = true;
  toast(r.msg || '已打开客户端，请在窗口里登录', true);
  stopWzPoll();
  w.poll = setInterval(async () => {
    if (!S.wizard || S.wizard !== w) { stopWzPoll(); return; }
    const L = await wzReadLogin(true);
    if (L && L.ok && L.account) stopWzPoll();
  }, 3000);
  renderWizard();
}
function renderWizard() {
  const w = S.wizard;
  const body = $('wz-body');
  clear(body);
  const steps = el('ol');
  WZ_STEPS.forEach((s, idx) => {
    steps.appendChild(el('li', { class: idx === w.step ? 'on' : (idx < w.step ? 'done' : '') },
      el('i', {}, String(idx + 1)), s.t));
  });
  clear($('wz-steps'));
  $('wz-steps').append(...steps.children);

  const sid = WZ_STEPS[w.step].id;
  $('wz-back').toggleAttribute('disabled', w.step === 0);
  const next = $('wz-next');
  clear(next);
  next.append(
    sid === 'done' ? '确认无误，建这张卡'
      : (sid === 'login' && w.m !== 'apikey'
        ? (w.login && w.login.ok && w.login.account ? '下一步' : '读取登录状态')
        : '下一步'));

  /* ---- ① 登录方式 ---- */
  if (sid === 'method') {
    const tabs = el('div', { class: 'mtabs' });
    for (const key of methodsFor(w.app)) {
      const b = el('button', { type: 'button', class: `mtab${w.m === key ? ' on' : ''}` }, METHODS[key].t);
      b.addEventListener('click', () => { w.m = key; renderWizard(); });
      tabs.appendChild(b);
    }
    for (const s of METHODS_SOON) {
      tabs.appendChild(el('button', {
        type: 'button', class: 'mtab soon', disabled: '',
        title: `${s.d}（还没接通）`,
      }, s.t));
    }
    body.append(
      el('p', {}, `用哪种方式给 ${w.app.name} 加账号？不同程序支持的方式不一样。`),
      tabs,
      el('div', { class: 'mdesc' }, METHODS[w.m].d),
      el('div', { class: 'hintline' },
        '灰色的是还没接通的方式（OAuth 授权 / Token·JSON），列出来只是让你知道有这条路。'));
  }

  /* ---- ③ 基本信息（上一步已经认出登的是谁，这里默认填好） ---- */
  if (sid === 'setup') {
    const nameIn = el('input', { value: w.name, placeholder: '账号名称，如 备用号 / 公司号' });
    nameIn.addEventListener('input', () => { w.name = nameIn.value; w.nameTouched = true; });
    const wdIn = el('input', { value: w.workdir, placeholder: '该账号默认打开的工作目录，D 盘也行（可留空）' });
    wdIn.addEventListener('input', () => { w.workdir = wdIn.value; });
    const L = w.login || {};
    const who = L.ok && L.account
      ? el('div', { class: 'hintline' },
          `这一步的名字是从登录状态里读出来的：${L.account}`
          + `${L.user_type ? ` · 套餐 ${L.user_type}` : ''}`
          + `${L.user_id ? ` · ID ${String(L.user_id).slice(0, 8)}…` : ''}`
          + (L.card ? `。这个号已经有卡片「${L.card}」了，建议别再建重复的。` : '。想改成别的名字直接编辑上面这格。'))
      : el('div', { class: 'hintline bad' },
          '没读到你登录了谁，所以名字还是占位的。可以回到上一步重新读，或先自己起个名，之后在卡片「账号设置」里改。');
    body.append(
      who,
      el('div', { class: 'row' }, el('label', {}, '名称'), nameIn),
      el('div', { class: 'row' }, el('label', {}, '工作目录'),
        el('div', { class: 'ctlrow browse' }, wdIn,
          browseBtn(() => wdIn.value, (v) => { wdIn.value = v; w.workdir = v; }))),
      el('p', { class: 'hintline' },
        '工作目录只是"这个账号打开时先进哪个项目文件夹"，放 D 盘完全可以。账号自己的档案（登录态、设置、缓存）在哪、要不要搬，下一步单独问。'));
  }

  /* ---- ④ 数据来源 + 怎么管 ---- */
  if (sid === 'data') {
    if (w.m === 'apikey') {
      body.append(
        el('p', {}, 'API Key 账号从空档开始，也不用选换号还是多开。'),
        el('div', { class: 'hintline' },
          '这种账号是"一个上游 + 一把 key"，登录态不在本地客户端里，所以没有对话记录可继承，'
          + '多开靠的是每个实例各自的目录。'));
    } else {
      const srcs = w.sources;
      if (!srcs) {
        body.appendChild(el('div', { class: 'hintline' }, '正在看本机有哪些档案、分别是谁在用…'));
      } else if (!w.app.isolatable) {
        /* 单窗口换号的产品：这台机器上就一份档案，账号在客户端里切。
           对话与记忆是所有账号共用的，不存在"这个账号自己的数据"，
           所以这一步不再让她挑目录——挑不出来，也不该挑。 */
        const prof = srcs.find((s) => s.kind === 'profile');
        const acct = (w.login && w.login.account) || w.name.trim();
        const claimer = (prof && prof.facts && prof.facts.claimed_by) || '';
        w.origin = 'adopt';
        w.mode = 'swap';
        w.claimer = claimer;
        body.append(
          el('p', {}, '这个程序在这台机器上只有一份档案，账号是在客户端里切换的。'),
          el('div', { class: 'hintline' },
            `档案位置：${prof ? prof.path : '（没登记到路径）'}`
            + (prof && prof.has_data ? ` · ${factsLine(prof)}` : '')),
          el('div', { class: 'hintline' },
            '对话历史、记忆、设置都存在这同一份档案里，所有账号共用：在客户端里换个号，'
            + '不会换出一套新对话，也不会把旧的隔开存。'
            + '所以这一步不问"要不要搬数据"——建这张卡不搬、不复制、不删任何文件，'
            + '只是记下"这份档案现在登的是哪个号"，好让额度过期提醒对得上人。'),
          el('p', { style: 'margin-top:14px' }, '那这张卡跟这份档案怎么对应？'));
        const pick = (v, title, desc, mark) => {
          const c = el('div', { class: 'choice', aria: { checked: (w.plan || 'new') === v } },
            el('div', {},
              el('div', { class: 't' }, title, mark ? el('span', { class: 'recmk' }, '推荐') : null),
              el('div', { class: 'd' }, desc)));
          c.addEventListener('click', () => { w.plan = v; renderWizard(); });
          return c;
        };
        body.appendChild(pick('new', `新建一张卡，登记为「${acct || '这个账号'}」`,
          claimer && claimer !== acct
            ? `原来那张「${claimer}」不删，标成"已让位"留着看历史额度；这份档案之后归新卡。`
            : '这份档案现在登的就是它，建完卡名字和登录态直接对得上。'));
        if (claimer && claimer !== acct) {
          body.appendChild(pick('rebind', `不新建，把「${claimer}」那张卡改绑成「${acct}」`,
            '一份档案挂一张卡。卡片名字跟着现在登录的号变，额度按当前读数重写。', true));
        }
        body.appendChild(el('div', { class: 'hintline' },
          '管理方式：这个程序会把第二次启动转交给已开窗口，不能同时多开，所以账号靠客户端里切换'
          + '（退出登录 → 登另一个号），一次一个。驾驶舱不会为了换号去关你的程序。'));
      } else {
        const prof = srcs.find((s) => s.kind === 'profile');
        const insts = srcs.filter((s) => s.kind === 'instance' && s.has_data);
        body.appendChild(el('p', {}, '这个账号打开时，要接着用你本机原来那份对话和记忆，还是从零开始？'));
        const mkChoice = (origin, title, desc, path, enabled, from) => {
          const c = el('div', {
            class: `choice${enabled ? '' : ' off'}`,
            aria: { checked: w.origin === origin || (w.origin === 'auto' && w.recommend === origin) },
          },
            el('div', {},
              el('div', { class: 't' }, title, w.recommend === origin ? el('span', { class: 'recmk' }, '推荐') : null),
              el('div', { class: 'd' }, desc),
              path ? el('div', { class: 'd' }, `位置：${path}`) : null));
          if (enabled) c.addEventListener('click', () => { w.origin = origin; w.from = from || ''; renderWizard(); });
          return c;
        };
        if (prof) {
          body.appendChild(mkChoice('adopt', '接着用本机原来那份',
            `${factsLine(prof)}。${prof.note}。不搬家、不复制：这张卡就挂在程序现在用的那份档案上，`
            + '以后换到别的号，是在它同级把它改名让位，随时换得回来。',
            prof.path, prof.has_data && !prof.junction, prof.path));
        }
        if (insts.length) {
          body.appendChild(mkChoice('copy', `从已有账号「${insts[0].name}」复制一份`,
            `${insts.map((s) => `${s.title || s.name}：${factsLine(s)}`).join('；')}。`
            + '整份复制过来，原来那张卡不动。想拿旧号的对话开一个新号时才选这个。',
            insts.length === 1 ? insts[0].path : '', true, insts[0].path));
        }
        body.appendChild(mkChoice('fresh', '从零开始，给它一个干净的空目录',
          '这个账号打开时没有任何历史对话与记忆。原来那份完全不动。',
          '', true));
        body.appendChild(el('p', { style: 'margin-top:14px' }, '这个账号怎么管？'));
        const iso = el('div', { class: 'choice', aria: { checked: w.mode === 'isolate' } },
          el('div', {},
            el('div', { class: 't' }, '独立数据目录（真多开）'),
            el('div', { class: 'd' }, '启动时传 --user-data-dir，可以同时开多个账号窗口。')));
        const swp = el('div', { class: 'choice', aria: { checked: w.mode === 'swap' } },
          el('div', {},
            el('div', { class: 't' }, '换号（一次一个账号）'),
            el('div', { class: 'd' }, '把默认档案目录指向这个账号（NTFS junction，零额外空间）。')));
        if (w.app.isolatable) iso.addEventListener('click', () => { w.mode = 'isolate'; renderWizard(); });
        else swp.addEventListener('click', () => { w.mode = 'swap'; renderWizard(); });
        body.append(
          el('p', { class: 'hintline' }, '这个程序实测支持独立数据目录，可以同时开多个账号窗口；只想逐个切换的话也可以选换号。'),
          iso, swp);
      }
    }
  }

  /* ---- ② 登录：先真的登进去，后面所有信息都从这次登录里读 ---- */
  if (sid === 'login') {
    if (w.m === 'apikey') {
      const up = w.up;
      const chips = el('div', { class: 'chips' });
      for (const p of PROVIDERS) {
        const b = el('button', { type: 'button', class: `chip${up.name === p.n ? ' on' : ''}` }, p.n);
        b.addEventListener('click', () => { up.name = p.n; up.base = p.b; renderWizard(); });
        chips.appendChild(b);
      }
      const f = (label, key, ph, type) => {
        const inp = el('input', { type: type || 'text', placeholder: ph, value: up[key] });
        inp.addEventListener('input', () => { up[key] = inp.value; });
        return el('div', { class: 'row' }, el('label', {}, label), inp);
      };
      const ta = el('textarea', { rows: '3', placeholder: '每行一个模型 ID，也可用逗号分隔' });
      ta.value = up.models;
      ta.addEventListener('input', () => { up.models = ta.value; });
      const fb = el('button', { type: 'button', class: 'btn sm' }, '从上游获取');
      fb.addEventListener('click', async () => {
        fb.disabled = true;
        const r = await invoke('action', { payload: { action: 'key_probe_models', base: up.base, key: up.key } });
        fb.disabled = false;
        if (!r.ok) { toast(`拉不到模型：${r.msg}`, false); return; }
        up.models = (r.models || []).join('\n');
        toast(r.msg, true);
        renderWizard();
      });
      body.append(
        el('p', {}, w.created
          ? '上游信息已记进台账，账号也建好了。'
          : '填上游地址与 Key。点「添加账号并打开」会一次做完：记进台账 → 建账号 → 切换并打开。'),
        el('div', { class: 'flabel' }, '上游'),
        chips,
        f('名称', 'name', '这个上游叫什么，会显示在台账里'),
        f('Base URL', 'base', 'https://…/v1'),
        f('API Key', 'key', 'sk-…', 'password'),
        el('div', { class: 'flabel' }, '模型列表（可留空，之后在「API」页补）'),
        ta,
        el('div', { class: 'row' }, el('label', {}, ''), el('span', { class: 'grow' }), fb),
        el('div', { class: 'hintline' },
          '这一步只把 key 记进台账，不动这个 Agent 的配置文件。'
          + '记好之后：在「API 与额度」那行可以查看 / 复制、加进反代池，'
          + '也可以点「接入」把模型直接写进 Qoder / WorkBuddy 的自定义模型配置（写之前自动备份，可逐条撤掉）。'));
    } else {
      const importing = w.m === 'import';
      body.append(
        el('p', {}, importing
          ? '这一步不用重新登录：你选的是「导入已有数据」，登录状态本来就在档案里。'
            + '我先读一下本机现在登的是谁，认出来就直接往下填。'
          : loginHint(w.app)),
        el('div', { class: 'ctlrow' },
          ...(importing ? [] : [btn('▶ 打开客户端登录', 'primary', wzOpenClient)]),
          btn(importing ? '读取登录状态' : '重新读取', 'sm', () => wzReadLogin(true)),
          btn('跳过此步', 'sm', () => { w.step += 1; renderWizard(); })));
      const L = w.login;
      if (!L) {
        body.appendChild(el('div', { class: 'hintline' },
          '还没读过登录状态。点「打开客户端」后会每 3 秒自动读一次，认出新的登录账号就自己进下一步；'
          + '你也可以在客户端里登录完再点「重新读取」。'));
      } else if (!L.ok) {
        body.appendChild(el('div', { class: 'hintline bad' },
          L.unsupported
            ? `${L.msg || '这个程序没有登记本机直读通道，登录态读不了'}。这一步跳过也行：后面自己起名字、自己填额度。`
            : `${L.err || L.msg || '读不到登录状态'}。这一步跳过也行：后面自己起名字、自己填额度。`));
      } else {
        body.append(
          el('ol', { class: 'wl' },
            el('li', { class: 'ok' }, `登录到了「${L.account}」${L.user_type ? `（套餐 ${L.user_type}）` : ''}`),
            el('li', { class: 'ok' }, `服务端读数：剩 ${nfmt(L.left || 0)} / 共 ${nfmt(L.total || 0)}`
              + `${L.expires_on ? ` · 到期 ${dtxt(L.expires_on)}` : ''} · 来源 ${L.source || '未知'}`),
            el('li', {}, L.auth_from ? `登录档案 ${L.auth_from}` : `ID ${String(L.user_id || '').slice(0, 8)}…`),
            L.card ? el('li', { class: 'ok' }, `这个号已经有卡片「${L.card}」了`) : null),
          el('div', { class: 'hintline' }, L.card
            ? '建议直接用那张卡，别再建一张——两张卡指向同一个号，额度会记成两份。'
            : '后面几步的名字、余额、逐笔额度都从这里读，不用你手抄。'));
      }
    }
  }

  /* ---- ⑤ 额度：只自动读（官网逐包 → 服务端 → 本机），界面上不手填 ---- */
  if (sid === 'quota') {
    w.credits = w.credits || [];
    if (!w.quotaTried) { w.quotaTried = true; wzLoadQuota(); }
    const W = w.web;
    const src = el('div', { class: 'hintline' });
    if (W && W.ok) {
      src.append(`官网读数：${W.account} · 剩 ${nfmt(W.left)} / 共 ${nfmt(W.total || 0)}`
        + ` · 逐笔 ${W.portions.length} 包 · 套餐 ${W.plan || '未知'}`);
      (W.groups || []).forEach((g) => {
        if (!g.n) return;
        src.appendChild(el('div', { class: 'orphan' },
          `${g.name}：剩 ${nfmt(g.left)} / 共 ${nfmt(g.total)}，${g.n} 笔`));
        const ul = el('ol', { class: 'wl wzpacks' });
        (g.items || []).forEach((r) => {
          const e = dtxt(r.expires_on);
          ul.appendChild(el('li', {},
            `${r.label} 剩 ${nfmt(r.left)} / ${nfmt(r.total)} · `
            + (e === '未给出' || e === '无限期' || e === '—' ? e : `到期 ${e}`)
            + `${r.status && r.status !== 'ACTIVE' ? ` · ${r.status}` : ''}`));
        });
        src.appendChild(ul);
      });
    } else if (W) {
      src.append(W.need_login
        ? `官网还没登录：${W.msg}`
        : `官网没读成：${W.msg || '未知原因'}`);
      if (w.app.web_quota) {
        src.appendChild(btn(W.need_login ? '打开官网登录窗口' : '再试一次', 'sm',
          () => (W.need_login ? wzWebLogin() : wzLoadQuota(true))));
      }
    } else {
      src.append(w.app.web_quota ? '正在读官网额度…' : '这个产品没有登记官网用量页，只读服务端与本机。');
    }
    body.append(...[
      el('p', {}, '额度这一页没有手填框——读到的样子就是卡片上的样子。三处都读不到就先跳过，'
        + '建好卡之后在「账号设置 → 额度与用量 → 截图识别」把官网积分页截一张图贴进去。'),
      el('div', { class: 'ctlrow' },
        btn('↻ 重新读一次额度', 'sm', () => wzLoadQuota(true),
          w.app.web_quota
            ? '一次读全：先从驾驶舱自己的官网登录窗口取这个号的会话，读个人主页的逐资源包明细（每包各自的到期时间只有官网有）；读不到再退服务端与本机。'
            : '这个产品没有登记官网用量页，直接读服务端与本机。'),
        pasteHint('quota')),
      src,
      (w.credits || []).length ? el('div', { class: 'flabel' }, `读到的逐笔额度（${w.credits.length} 笔）`) : null,
      (w.credits || []).length ? el('div', { class: 'qitems' }, ...(w.credits || []).map((c) => el('div', { class: 'qitem' },
        el('div', { class: 'qitop' }, el('b', {}, c.label || '未命名')),
        el('div', { class: 'qimid' },
          el('span', {}, `到期时间 ${dtxt(c.expires_on)}`),
          el('span', { class: 'qistat' }, `剩 ${nfmt(c.left)} / 共 ${nfmt(c.total)}`))))) : null,
      el('div', { class: 'hintline' },
        '这些会被写进这张卡，卡片与顶栏「临期 N」按每一笔各自的到期日提醒。不想现在读就点「下一步」跳过。'),
    ].filter(Boolean));
  }

  /* ---- ⑥ 确认：把这一趟读到的东西摊开，点头才真的建卡 ---- */
  if (sid === 'done') {
    const L = w.login || {};
    const chosen = w.origin === 'auto' ? (w.recommend || 'fresh') : w.origin;
    const src = (w.sources || []).find((s) => s.origin === chosen);
    const n = (w.credits || []).length;
    body.append(
      el('p', {}, '确认一下。点「确认无误，建这张卡」之后，驾驶舱只在自己的数据根里写一个账号文件——'
        + '不搬、不复制、也不删这个程序的任何档案。'),
      el('ol', { class: 'wl' },
        el('li', { class: L.ok ? 'ok' : '' }, `账号：${w.name}${L.ok ? `（登录账号 ${L.account}）` : '（没读到登录账号）'}`),
        el('li', {}, `工作目录：${w.workdir || '（留空，用程序默认）'}`),
        el('li', {}, `数据来源：${ORIGIN_TXT[chosen] || chosen}${src && src.path ? ` · ${src.path}` : ''}`),
        w.claimer && w.claimer !== w.name.trim()
          ? el('li', {}, `原来认领这份档案的「${w.claimer}」会被标成"已让位"，卡片留着、历史额度还能看（也不会动它的文件）`)
          : null,
        el('li', {}, `管理方式：${w.mode === 'isolate' ? '独立数据目录（真多开）' : '换号（一次一个账号）'}`),
        el('li', { class: n ? 'ok' : '' }, n
          ? `额度：逐笔 ${n} 包，最早到期 ${dtxt((w.credits || []).map((c) => c.expires_on).filter(Boolean).sort()[0] || '')}`
          : '额度：这步没填，之后在卡片「账号设置 → 额度与用量」里补'),
        el('li', {}, `登录方式：${METHODS[w.m].t}`)),
      el('div', { class: 'hintline' },
        '建好之后卡片上只有一个主按钮，它按"这个账号现在能不能开"自动变：已运行→「■ 结束」；'
        + '未运行但不是当前账号→「▶ 切到本账号并打开」；未运行且是当前账号→「▶ 启动」。'
        + '其余都在「账号设置」里：额度、逐包明细、工作目录、备份恢复、改名、删除。'));
  }
}
// 第 5 步的自动取数：官网逐包优先（它有每个资源包的到期日），没有就退回服务端/本机
/* 向导第②步已经认出登进来的是哪个号了：官网会话就按这个 userId 开格子，
   这样建完卡第一次「刷新额度」用的是同一份会话，不用登第二次。 */
const wzUid = () => (S.wizard && S.wizard.login && S.wizard.login.user_id) || '';
async function wzLoadQuota(force) {
  const w = S.wizard;
  if (!w) return;
  // 和卡片上那颗「刷新额度」同一套顺序：官网逐包读得到就用它（每包自己的到期时间只有那里有），
  // 读不到当场接着退到服务端/本机，不再要人多点一颗按钮
  let webOk = false;
  if (w.app.web_quota && (force || !w.web)) {
    w.web = null; renderWizard();
    const r = await run('web_fetch', { app: w.app.id, user_id: wzUid() });
    if (S.wizard !== w) return;
    webOk = !!(r && r.ok);
    w.web = webOk ? r.data : { ok: false, need_login: !!(r && r.need_login), msg: (r && r.msg) || '读不到' };
    if (webOk) {
      const ps = (r.data && r.data.portions) || [];
      if (ps.length) {
        w.credits = ps.map((p) => ({ label: p.label, left: p.left, total: p.total, expires_on: p.expires_on }));
      }
    }
    renderWizard();
  }
  if (!webOk && !w.credits.length) {
    const L = await run('machine_login', { app: w.app.id, refresh: true });
    if (S.wizard !== w) return;
    w.login = L;
    if (L && L.ok) {
      w.credits = (L.portions || []).map((p) => ({ label: p.label, left: p.left, total: p.total, expires_on: p.expires_on }));
    }
  }
  renderWizard();
}
async function wzWebLogin() {
  const w = S.wizard;
  const r = await run('web_login', { app: w.app.id, user_id: wzUid() });
  if (r && r.ok) afterWebLogin(w.app.id, '', wzUid());
  else toast((r && r.msg) || '打不开官网窗口', false);
}
// 一份档案只能被一张卡认领：旧卡还挂着时，把它改名改绑到当前登录的号，而不是再建一张
async function wzRebind(oldName, newName) {
  const w = S.wizard;
  const acct = w && w.login && w.login.account ? w.login.account : '';
  if (!await askConfirm(
    `把卡片「${oldName}」改绑成${acct ? `「${acct}」` : '当前登录的账号'}？\n\n`
    + '不新建卡片、不动任何档案文件：只是把那张卡的名字与它绑定的 userId 换成现在登录的这个号，'
    + '再把刚读到的额度写进去。\n原来记在「' + oldName + '」名下的额度会被当前读数覆盖。',
    { title: '改绑到当前登录的账号', okText: '改绑' })) return;
  const r = await run('rebind_instance', {
    app: w ? w.app.id : S.cur, name: oldName,
    to: (newName && newName.trim() && newName.trim() !== '账号1') ? newName.trim() : (acct || oldName),
    account: acct,
  });
  if (!r || !r.ok) { toast((r && r.msg) || '改绑失败', false); return; }
  toast(r.msg, true);
  if (w) closeWizard();
}
async function wzCreate() {
  const w = S.wizard;
  if (!w) return;
  const nm = w.name.trim();
  if (!nm) { toast('名称不能为空', false); return; }
  const chosen = w.origin === 'auto' ? (w.recommend || 'fresh') : w.origin;
  const payload = {
    app: w.app.id, name: nm, workdir: w.workdir,
    origin: w.m === 'apikey' ? 'fresh' : chosen,
  };
  // 这份档案原来被别的卡认领：新卡建起来时让它让位（只打标记，不碰文件）
  if (w.claimer && w.claimer !== nm) payload.take_over = w.claimer;
  const cr = (w.credits || []).filter((c) => (c.label || '').trim() || c.left != null || c.total != null);
  if (cr.length) payload.credits = cr;
  for (const k of ['total', 'left', 'exp']) {
    if (w[k]) payload[{ total: 'credits_total', left: 'credits_left', exp: 'expires_on' }[k]] = w[k];
  }
  // 把这一趟读到的身份一起写进去：卡片一建出来就知道自己的数是谁的、从哪来的
  const W = w.web && w.web.ok ? w.web : null;
  const L = w.login && w.login.ok ? w.login : null;
  if (W) {
    Object.assign(payload, {
      quota_user_id: W.user_id, quota_account: W.account, quota_plan: W.plan,
      quota_source: 'web', quota_groups: W.groups, quota_plan_name: W.plan_name,
    });
  } else if (L) {
    Object.assign(payload, {
      quota_user_id: L.user_id, quota_account: L.account, quota_plan: L.user_type,
      quota_source: L.source, quota_live_error: L.live_error || '',
    });
  }
  let r = await run('new_instance', payload);
  if (r.need_confirm === 'adopt') {
    const go = await askConfirm(
      `账号「${nm}」就用你本机原有的这份档案。\n\n`
      + `目录：${r.source}（${r.mb} MB）\n\n`
      + '数据不搬走、也不复制第二份，这个账号的名字就挂在这个目录上。'
      + '以后换到别的账号，是在它的同级把它改名让位，随时换得回来。\n'
      + '如果这个路径看着不像这个程序的档案（比如是另一个产品一直在用的目录），请点「取消」。',
      { title: '确认用本机原有档案？', okText: '就用这个目录' });
    if (!go) return;
    r = await run('new_instance', { ...payload, confirm_adopt: true });
  }
  if (!r.ok) return;
  stopWzPoll();
  if (w.mode === 'isolate') await run('launch', { app: w.app.id, name: nm });
  else await swapSetAndLaunch(w.app.id, nm);
  closeWizard();
  toast(`已建好「${nm}」。${cr.length ? `额度记了 ${cr.length} 笔。` : '额度还没填，可在账号设置里补。'}`, true);
}
async function wizardNext() {
  const w = S.wizard;
  if (!w) return;
  const sid = WZ_STEPS[w.step].id;
  if (sid === 'login') {
    if (w.m === 'apikey') {
      const up = w.up;
      if (!up.base.trim()) { toast('Base URL 不能为空', false); return; }
      if (!up.key.trim()) { toast('API Key 不能为空', false); return; }
      const r = await run('key_upsert', {
        item: {
          name: up.name.trim() || up.base.replace(/^https?:\/\//, '').split('/')[0],
          kind: 'relay', homepage: '',
          keys: [{
            id: `k${Date.now().toString(36)}${Math.random().toString(36).slice(2, 6)}`,
            label: `${w.app.name}·${w.name.trim()}`, base_url: up.base.trim(), key: up.key.trim(),
            models_list: up.models.trim(), use_in_proxy: false,
          }],
        },
      });
      if (!r.ok) return;
    } else if (!(w.login && w.login.ok && w.login.account)) {
      const L = await wzReadLogin(false);
      if (!(L && L.ok && L.account)) {
        toast('还没读到你登录了谁。可以先跳过这一步，之后自己起名、自己填额度。', false);
        return;
      }
    }
    w.step += 1; renderWizard(); return;
  }
  if (sid === 'setup') {
    if (!w.name.trim()) { toast('名称不能为空', false); return; }
    w.step += 1; renderWizard(); return;
  }
  if (sid === 'data') {
    if (w.plan === 'rebind' && w.claimer) { await wzRebind(w.claimer, w.name.trim()); return; }
    w.step += 1; renderWizard(); return;
  }
  if (sid === 'done') { await wzCreate(); return; }
  w.step += 1;
  renderWizard();
}
function closeWizard() {
  stopWzPoll();
  $('wizard').hidden = true;
  S.wizard = null;
}

/* ---------------- 帮助页 ---------------- */
const HELP = [
  {
    t: '多开是怎么做到的，为什么有的能多开有的不能',
    h: `<p>桌面端（Electron/Chromium 系）靠启动参数 <code>--user-data-dir=实例目录</code> 让每个账号拥有独立档案，互不干扰，可以同屏开多个窗口。</p>
      <p>但<b>不是所有程序都配合</b>。不少产品启动时会申请"单实例锁"，第二次启动会把请求转交给已经开着的窗口、自己退出，参数被整个丢弃——这时表面上"启动成功"，实际唤出来的还是原来那个账号。</p>
      <p>本工具的做法：启动后<b>真的去进程表里核对</b>有没有带本实例目录的进程。核对失败就如实告诉你不支持同时多开，并把这个产品的能力记下来（顶栏「重置判定」可清掉重来），卡片自动改成换号入口。</p>`,
  },
  {
    t: '不能多开的产品，怎么用多个账号',
    h: `<p>分两种，界面会直接写明是哪种，因为要做的事完全不一样：</p>
      <h4>① 账号共用一份档案（Qoder 系 / WorkBuddy / ZCode / TRAE CN 这类）</h4>
      <p>这类产品在这台机器上只有一份档案，几个号都住在里面。切换账号<b>就在程序里做</b>：退出登录 → 登另一个号。
      卡片上的「▶ 启动客户端」只负责把程序打开——不搬文件、不挂链接，也不会关掉你正在用的程序。
      哪个号登进来了，驾驶舱从登录态里认出来，那张卡自己标上「本机登录中」，额度与临期提醒跟着它走。</p>
      <h4>② 每个账号一份独立目录、但只能一次开一个</h4>
      <p>用<b>换号</b>：把该程序的默认档案目录改成一个指向账号目录的 NTFS junction，原目录备份为 <code>*.cockpit-original</code>。
      之后你从开始菜单、任务栏任何入口打开，都是这个账号。</p>
      <ul>
        <li>零额外磁盘占用——不会存两份数据</li>
        <li>切换前要先关掉该程序的窗口（文件被占住时会点名是哪个进程，确认后驾驶舱可以替你结束它们）</li>
        <li>「还原默认」一键换回原来的账号目录</li>
      </ul>
      <p>侧栏上带 <code>⇄账号名</code> 标记的，就是当前正在使用的账号。</p>`,
  },
  {
    t: '额度怎么显示、从哪来',
    h: `<p>额度这块照 WorkBuddy 官方用量弹窗画：<b>外层是积分明细</b>——大字总剩余，底下按官方口径分成「套餐基础积分 / 平台奖励积分 / 购买积分」，读不到的家归进「其他」；<b>每一档可以点「查看明细」</b>，进去是「有效期内 / 已过期」两个页签，逐笔资源包各自写着到期时间（精确到分钟）、已用多少、还剩多少。卡片上是同一套数据的紧凑版。临期的汇总到顶栏「临期 N」徽章，7 天内启动时弹窗提醒一次；关窗后驻留托盘继续监控。</p>
      <h4>数从哪来（界面上没有手填框）</h4>
      <ul>
        <li><b>本机直读</b>：Qoder 系 / WorkBuddy / ZCode / TRAE CN / AStudio / MiniMax 解客户端自己存的积分，不联网、不改任何产品文件，逐笔到期日一起带回来。</li>
        <li><b>截图识别</b>：只有官网才显示数的那种，积分页截图 Ctrl+V 贴进「账号设置 → 额度与用量」，认成逐笔积分后核对保存。</li>
        <li><b>接口抓取</b>：OpenAI 官方 billing，或任何返回 JSON 的额度 URL + 取值路径（如 <code>data.remaining</code>）。</li>
      </ul>
      <p>配了来源的账号每 30 分钟后台自动刷一次（间隔在 设置 → 额度提醒），卡片显示上次抓取时间。</p>`,
  },
  {
    t: 'API 与额度页 / 本地反代',
    h: `<p>「API 与额度」用来管各家官网 API 与中转站的 Key：Base URL、Key（列表里打码，点复制取全文）、额度与过期日，一键 <code>GET /v1/models</code> 测试。</p>
      <p>登记不必一条条填表：页顶「快速登记」把官方或中转站发你的那段信息原样贴进来（截图也行，Ctrl+V 直接粘），程序先按本机规则读，认不出 key 或地址才交给多模态模型，站点、端点、每条 key 与用途名当场建好；再点 key 行的「模型」向上游要清单，勾多少登记多少，「测试」通了就能用。</p>
      <p>本地反代在 <code>127.0.0.1:8788</code> 做 OpenAI 兼容转发：替换 Authorization 头、保留流式输出，并记录每次请求用了哪个上游。编制在账号库下方「本地反代」面板里，是三段式——</p>
      <ul>
        <li><b>来源</b>：某个 Agent 的某个账号（要有 Token），或「API 与额度」里登记的某个 Key。</li>
        <li><b>池子</b>：把来源勾进一个池，就是一个可轮询的上游集合。成员可单独指定上游地址，留空用全局上游。</li>
        <li><b>分发 Key</b>：生成 <code>ac-…</code> 形式的 Key，指定它只能走哪些池，复制出去给某个客户端当 API Key 用。停用或删除，那个客户端立刻被拒。</li>
      </ul>
      <p>请求带着分发 Key 进来 → 只在它授权的池子里挑上游 → 换成该上游自己的凭据发出去。一个池都没建时退回老行为：所有填了 Token 的账号一起轮询。</p>`,
  },
  {
    t: '自动发现与数据存放',
    h: `<p>启动时会扫开始菜单与桌面的快捷方式，命中 AI 关键词或装在 <code>D:\\Artificial_Intelligence</code> 下的程序自动收录（侧栏标「自动」），并实测它是否支持独立目录隔离。新装软件后点左侧栏的「重新扫描」即可。</p>
      <p>所有数据只在本机的数据根（默认 <code>~/.agent-cockpit</code>，可在 设置→存储 迁到别的盘）：<code>instances/&lt;产品&gt;/&lt;账号&gt;/</code> 只放你另建的账号（复制或全新空档），接管本机原有档案的账号数据仍留在程序自己的默认位置，<code>backups/</code> 是 zip 备份，<code>shared/</code> 是多账号共用的可再生大文件，<code>config.json</code> 存程序列表、换号记录与反代配置，<code>vault.json</code> 是加密后的 Key（DPAPI，可选再加一层主密码）。反代与界面只绑 127.0.0.1。</p>`,
  },
  {
    t: '新装的 Agent 怎么加进列表',
    h: `<p>三条路，按省事程度排：</p>
      <ul>
        <li><b>自动扫描</b>：装完软件点左侧栏「重新扫描」。它在开始菜单和桌面找快捷方式，命中 AI 关键词、或装在 <code>D:\\Artificial_Intelligence</code> 下的直接收录。</li>
        <li><b>手动登记</b>：侧栏下方「＋ 添加程序」（设置 → 高级 里同一个入口）。只需可执行文件路径，不改动该程序任何文件；绿色版、没建快捷方式、名字不含关键词的都走这里。</li>
        <li><b>隐藏误收</b>：左侧列表右键某程序 →「隐藏程序」，它就从侧栏消失但数据不动，随时在 设置 → 高级 → 已隐藏的程序 里恢复。手动添加的可以直接「从列表移除」。</li>
      </ul>
      <p>加进来之后能不能多开，是内核<b>实测</b>出来的（同目录有 <code>resources.pak</code> + <code>locales</code> 才认 Chromium 内核），判定错了可在设置里「重置多开判定」重来。</p>`,
  },
  {
    t: '换号后程序空白/黑屏怎么自救',
    h: `<p>这一条只关乎上面第②种「账号各有一份目录」的产品：换号会把程序的默认档案目录改成一个指向账号目录的 junction，原档案改名备份。如果切到一个<b>还没登录过的空账号</b>，程序打开就是未登录或空白界面——WorkBuddy 这类程序会直接显示<b>黑屏</b>。这不是坏了，是档案被换走了。</p>
      <ul>
        <li>回到驾驶舱，在该账号卡片上点<b>「还原默认」</b>即可恢复原档案（需先关掉该程序窗口）</li>
        <li>驾驶舱没开时手动恢复：先删掉 <code>~/.程序名</code> 这个 junction，再把 <code>~/.程序名.cockpit-original</code> 改回 <code>~/.程序名</code></li>
        <li>现在切到空账号前会先弹确认，换号成功的提示里也会写明怎么退回</li>
      </ul>`,
  },
];
/* 帮助与教程：并入 设置 的一个分组（原来它自己占一页，侧栏里点一下才看得到，
   跟「关于」这类内容重复分层）。两个动作按钮也在这里就地绑，不再依赖静态元素。 */
function renderHelp(body, panel) {
  body.appendChild(panel('帮助与教程',
    el('div', { class: 'ctlrow' },
      btn('重放新手引导', 'primary sm', startTour, '一步一步把主界面指给你看'),
      btn('重置多开判定', 'sm', () => run('reset_caps'),
        '清掉实测记录，下次启动账号时重新判定该产品能不能多开'))));
  for (const sec of HELP) {
    const box = el('div', { class: 'html' });
    // 静态内置文案，非用户输入
    box.innerHTML = sec.h;
    body.appendChild(el('section', { class: 'panel' }, el('h2', {}, sec.t), box));
  }
  body.appendChild(el('div', { class: 'hintline' },
    '一句话上手：选 Agent → ＋ 添加账号 → 按向导登录并读额度 → 以后每天只看顶栏有没有「临期 N」徽章、以及有没有弹提醒。'));
}

/* ---------------- 新手引导 ---------------- */
const TOUR = [
  { t: '落地页是首页', s: '#home-body', x: '一进来先看「今天要处理什么」。右边两颗按钮是这台机器上最常用的两个动作：一键刷新全部额度（把每个 Agent 每个账号的额度都读一遍，进度就长在它下面）和一键签到（今天能领的一次领掉，领之前会把清单摊给你确认）。往下依次是临期积分、今天还能领的、我的 Agent——点任意一行就进它自己的页面。' },
  { t: '左边是 Agent 列表', s: '#applist', x: '本机已装的 AI 程序都在这里。绿点表示它正在运行；⇄ 标记是当前在用的号（账号共用一份档案的产品按客户端里登着谁算）；徽章「多开」＝能同时开多个账号窗口，「换号」＝一次一个。' },
  { t: '新装的程序怎么加进来', s: '#btn-add-app', x: '有开始菜单/桌面快捷方式的，点左侧「重新扫描」自动收录；绿色版、没快捷方式、名字里不含 AI 关键词的，点这里手动贴可执行文件路径登记。误收的右键「隐藏程序」，随时可在设置里恢复。' },
  { t: '中间是每个账号', s: '#cards', x: '一个账号一份档案（或几家共用一份）。卡片只放高频动作：这个号能不能开、怎么开——可多开的产品是「启动」，账号共用一份档案的产品是「启动客户端」，正在跑的是「结束」。' },
  { t: '其余设置都在账号内部', s: '#cards', x: '点卡片上的「账号设置」或卡片表头进入「账号信息」，点卡片中间那块额度直接进「额度与用量」。左侧六个分组按读得顺的顺序排：账号信息（哪家的号、叫什么、登录状态、档案从哪来）、额度与用量（逐笔明细 → 数据从哪来 → 立即取数）、运行与换号（多开能力 → 启动/结束 → 切换账号或设为当前/还原默认）、目录与数据（路径 / 空间 / 备份与恢复）、凭据与反代（API Token、反代可用性）、删除账号。一项一行，每行下面有小字说明它是干什么的。' },
  { t: '添加账号用向导', s: '#btn-new', x: '点它会走六步，顺序照着你真实的登录过程排：① 选登录方式（官方登录 / 导入已有数据 / API Key，灰色的是还没接通的）→ ② 打开客户端去登录，驾驶舱盯着认出你登成了谁 → ③ 基本信息（名字直接从登录状态读出来填好）→ ④ 数据来源（要不要接着用本机原来那份对话与记忆、换号还是独立目录多开）→ ⑤ 额度（先读官网逐资源包，再退到服务端与本机）→ ⑥ 确认，点「确认无误，建这张卡」才真的动手。' },
  { t: '额度过期看这里', s: 'header', x: '顶栏的「临期 N」徽章汇总所有临期赠送额度（30 天内黄、7 天内红），点一下看明细；7 天内的启动时会弹窗提醒一次。关窗后程序驻留托盘，每 30 分钟自动抓一次额度。' },
  { t: 'Key 与反代', s: '#viewlist', x: '「API 与额度」管各家官网/中转站 Key；本地反代在 127.0.0.1:8788 做 OpenAI 兼容转发。在「本地反代」面板里把账号/Key 归进池子，再生成分发 Key 给客户端用。' },
];
function startTour() {
  S.tour = { i: 0 };
  $('tour').hidden = false;
  renderTour();
}
function renderTour() {
  const step = TOUR[S.tour.i];
  $('tour-title').textContent = step.t;
  $('tour-text').textContent = step.x;
  $('tour-idx').textContent = `${S.tour.i + 1} / ${TOUR.length}`;
  const nx = $('tour-next');
  clear(nx);
  nx.append(S.tour.i === TOUR.length - 1 ? '开始使用' : '下一步');
  const target = document.querySelector(step.s);
  if (target) {
    const r = target.getBoundingClientRect();
    $('tour-card').style.top = `${Math.min(window.innerHeight - 120, Math.max(120, r.bottom + 140))}px`;
  }
}
function endTour() {
  $('tour').hidden = true;
  localStorage.setItem('cockpit-toured', '1');
  S.tour = null;
}

/* ---------------- 设置页 ---------------- */
const SET_SECTIONS = [
  ['profile', '个人资料'], ['general', '常规'], ['appearance', '外观'],
  ['quota', '额度提醒'], ['proxy', '本地反代'], ['ai', 'AI 识别'],
  ['storage', '存储'], ['security', '凭据安全'], ['advanced', '高级'],
  ['help', '帮助'], ['about', '关于'],
];
let setSection = 'general';
// 设置页整块内容会随 4 秒刷新重建，滚动容器的高度瞬间塌回顶部，不自己记住就滚不动
let setScroll = 0;
let setLast = '';

async function setSetting(section, key, value) {
  S.data.settings[section][key] = value;
  applyAppearance(S.data.settings.appearance);
  const r = await invoke('set_settings', { patch: { [section]: { [key]: value } } });
  S.data.settings = r;
  applyAppearance(r.appearance);
  schedulePoll();
  renderSettings();
}

function row(label, desc, ctl) {
  // 长说明收进 label 旁的问号圈（悬停/单击看全文），行内只留短句，版面不再被说明文字顶得忽高忽低
  const long = typeof desc === 'string' && desc.length > DESC_INLINE_MAX;
  return el('div', { class: 'setrow' },
    el('div', {},
      el('div', { class: 'lab' }, label, long ? qhelp(desc) : null),
      desc && !long ? el('div', { class: 'desc' }, desc) : null),
    el('div', { class: 'ctl' }, ctl));
}
function segCtrl(opts, value, pick) {
  const g = el('div', { class: 'seg' });
  for (const [v, t] of opts) {
    const b = el('button', { aria: { pressed: String(v === value) }, type: 'button' }, t);
    b.addEventListener('click', () => pick(v));
    g.appendChild(b);
  }
  return g;
}
function switchCtrl(value, pick) {
  const s = el('button', { class: 'switch', role: 'switch', aria: { checked: String(!!value) }, type: 'button' },
    el('i'));
  s.addEventListener('click', () => pick(!value));
  return s;
}
function numCtrl(value, min, max, pick) {
  const n = el('input', { type: 'number', value: String(value ?? ''), min: String(min), max: String(max) });
  n.addEventListener('change', () => {
    const v = Number(n.value);
    if (Number.isFinite(v)) pick(Math.min(max, Math.max(min, v)));
  });
  return n;
}
function textCtrl(value, ph, pick, wide) {
  const t = el('input', { type: 'text', value: value || '', placeholder: ph || '', style: wide ? 'width:320px' : null });
  t.addEventListener('change', () => pick(t.value));
  return t;
}
function textCtrlId(id, value, ph, pick, wide) {
  const t = textCtrl(value, ph, pick, wide);
  t.id = id;
  return t;
}
function actBtn(label, fn) { return btn(label, 'sm', fn); }

/* ---------------- 设置 · 存储 / 凭据安全 ---------------- */

const mbText = (n) => (n >= 1024 ? `${(n / 1024).toFixed(2)} GB` : `${Math.round(n)} MB`);

async function loadStorage(sizes, refresh) {
  if (S.storageBusy) return;
  S.storageBusy = true;
  S.storageError = '';
  try {
    const r = await invoke('action', {
      payload: { action: 'storage_report', sizes: !!sizes, refresh: !!refresh },
    });
    if (r && r.counting) {
      // 后台线程还在数（WorkBuddy 一家十几万个文件，可能要一两分钟），有界轮询，别把界面卡住
      S.storageCounting = (S.storageCounting || 0) + 1;
      if (S.storageCounting === 1) toast('正在后台统计各账号占用，数完自动填进来（可能要一两分钟）', true);
      if (S.storageCounting <= 120) setTimeout(() => loadStorage(true, false), 3000);
      return;
    }
    S.storageCounting = 0;
    if (r && r.ok) { S.storage = r.report; S.junk = r.junk || []; }
    else S.storageError = (r && r.msg) || '读取占用失败';
  } catch (e) {
    S.storageError = String(e);
  } finally {
    S.storageBusy = false;
    renderSettings();
  }
}

async function loadVault() {
  if (S.vaultBusy) return;
  S.vaultBusy = true;
  S.vaultError = '';
  try {
    const r = await invoke('action', { payload: { action: 'vault_state' } });
    if (r && r.ok) S.vault = r;
    else S.vaultError = (r && r.msg) || '读取保管箱状态失败';
  } catch (e) {
    S.vaultError = String(e);
  } finally {
    S.vaultBusy = false;
    renderSettings();
  }
}

function pwCtrl(ph, w) {
  return el('input', { type: 'password', placeholder: ph || '', style: `width:${w || 200}px` });
}

/* ---------------- 设置 · Windows 集成（桌面/开始菜单快捷方式） ---------------- */

async function loadShell() {
  if (S.shellBusy) return;
  S.shellBusy = true;
  S.shellError = '';
  try {
    const r = await invoke('action', { payload: { action: 'shell_shortcut_status' } });
    if (r && r.ok) S.shell = r;
    else S.shellError = (r && r.msg) || '读取快捷方式状态失败';
  } catch (e) {
    S.shellError = String(e);
  } finally {
    S.shellBusy = false;
    renderSettings();
  }
}

function shellSection(body, panel, row, actBtn, btn) {
  const W = S.shell;
  if (!W) {
    // 状态要查注册表 + 读现有 .lnk，异步拉一次；拉之前别反复触发
    if (!S.shellBusy && !S.shellError) loadShell();
    body.appendChild(panel('Windows 集成',
      el('div', { class: 'empty' }, S.shellBusy ? '正在读快捷方式状态…'
        : (S.shellError ? `没读到：${S.shellError}` : '正在读快捷方式状态…')),
      actBtn('重新读取', () => loadShell())));
    return;
  }
  const d = W.desktop, s = W.start;
  const state = (it) => it.exists
    ? (it.ours ? '已创建，指向本程序' : `已有同名项，但指向：${it.target || '读不出来'}`)
    : '尚未创建';
  const label = (it, base) => it.exists ? '已创建 · 覆盖重建' : base;
  const makeBtn = (kind, it, base) => btn(label(it, base), 'sm', async () => {
    if (it.exists && !it.ours) {
      // 名字撞了别人：覆盖会把那个程序在桌面/开始菜单里的入口换掉，必须问清楚
      const sure = await askConfirm(
        `这个位置已经有一个同名快捷方式，且指向的不是本程序：\n${it.path}\n现在的目标：${it.target || '读不出来'}\n\n`
        + '覆盖只会改这条快捷方式本身，不动那个程序。继续？',
        { danger: true, okText: '覆盖重建' });
      if (!sure) return;
    }
    const r = await run('shell_shortcut_create', { kind });
    if (r && r.ok) loadShell();
  });
  const any = d.exists || s.exists;
  const lines = [];
  if (d.exists) lines.push(`桌面：${d.path}（目标：${d.target || '读不出来'}）`);
  if (s.exists) lines.push(`开始菜单：${s.path}（目标：${s.target || '读不出来'}）`);
  body.appendChild(panel('Windows 集成',
    el('p', { style: 'font-size:12.5px;color:var(--text-muted);margin:0 0 10px' },
      `本程序是便携 exe，放到哪都能跑，但也因此不在系统里留任何入口。下面三个动作只生成/删除指向 ${W.exe} 的快捷方式（.lnk），不复制、不移动程序本身。`),
    row('创建桌面快捷方式',
      `实际落点：${d.path}；当前：${state(d)}。桌面位置按注册表 User Shell Folders 解析（OneDrive 重定向也认），图标用程序自带，工作目录设为 exe 所在目录。`,
      makeBtn('desktop', d, '创建桌面快捷方式')),
    row('添加到开始菜单',
      `实际落点：${s.path}；当前：${state(s)}。开始菜单位置同样走注册表解析，搜索程序名即可找到。`,
      makeBtn('startmenu', s, '添加到开始菜单')),
    row('移除以上快捷方式',
      any
        ? `只删名为「Agent 驾驶舱」且目标确实是 ${W.exe} 的 .lnk，删除前先备份到数据根 shell-link-backups/。桌面上其它东西一律不碰。`
        : '两处都还没有本程序的快捷方式，没什么可移除的。',
      any
        ? btn('移除以上快捷方式', 'danger sm', async () => {
          // 破坏性操作：确认框里先把要删的路径和各自指向的目标原样列出来
          const sure = await askConfirm(
            `将校验并删除以下快捷方式（只删目标指向本程序的）：\n\n${lines.join('\n')}\n\n`
            + '删除前会先备份到数据根的 shell-link-backups/。确认继续？',
            { danger: true, okText: '删除' });
          if (!sure) return;
          const r = await run('shell_shortcut_remove', { confirm: true });
          if (r && r.ok) loadShell();
        })
        : el('span', { class: 'kv' }, '—'))));
}

function storageSection(body, panel, row, actBtn) {
  const R = S.storage;
  if (!R) {
    if (!S.storageBusy && !S.storageError) loadStorage(false);
    body.appendChild(panel('数据放在哪',
      el('div', { class: 'empty' }, S.storageBusy ? '正在读…'
        : (S.storageError ? `没读到：${S.storageError}` : '正在读…')),
      actBtn('重新统计', () => loadStorage(true, true))));
    return;
  }
  const note = (t) => el('p', { style: 'font-size:12.5px;color:var(--text-muted);margin:0 0 10px' }, t);

  // 1) 位置与盘况
  const dtb = el('tbody');
  for (const d of (R.drives || [])) {
    dtb.appendChild(el('tr', {},
      el('td', { class: 't' }, `${d.drive}:  （${d.fs}${d.ntfs ? '' : ' · 不能放账号数据'}）`),
      el('td', {}, `${d.total_gb} GB`),
      el('td', {}, `${d.free_gb} GB`),
      el('td', {}, d.drive === R.drive ? chip('数据在这里', 'on') : (d.drive === R.sys_drive ? chip('系统盘', '') : ''))));
  }
  body.appendChild(panel('数据放在哪',
    row('驾驶舱的数据根', '`.agent-cockpit` 是驾驶舱自己的落盘位置：config.json（程序列表/换号记录）、instances/<产品>/<账号> （**只有你另建的账号**才住这里：复制或全新空档）、backups（zip 快照）、shared（多账号共用的大文件）、vault.json（加密后的 Key）。你本机原来那份档案**不搬到这里**——接管只是给它登记一个账号名，数据留在程序的默认位置，换号时也只在同级改名让位。早期版本把接管进来的档案搬进 instances/ 是设计错误，账号设置里可以一键归位。',
      el('div', { class: 'kv' }, R.root)),
    row('所在盘剩余空间', `现在这个盘还剩 ${R.free_gb} GB；系统盘（${R.sys_drive}:）还剩 ${R.sys_free_gb} GB。账号数据动辄几个 GB，放系统盘很快就挤爆。`,
      el('span', { class: 'kv' }, `${R.drive}: · 剩 ${R.free_gb} GB`)),
    el('div', { class: 'tablewrap' },
      el('table', { class: 'tbl' },
        el('thead', {}, el('tr', {},
          el('th', { class: 'col-app' }, '磁盘'),
          el('th', { style: 'width:18%' }, '总容量'),
          el('th', { style: 'width:18%' }, '剩余'),
          el('th', { style: 'width:22%' }, ''))), dtb))));

  // 2) 搬到别的盘
  const sug = R.suggested ? R.suggested.path : 'D:\\AgentData\\.agent-cockpit';
  const target = el('input', { type: 'text', value: R.is_default ? sug : sug, style: 'width:340px' });
  const go = async () => {
    const t = target.value.trim();
    if (!t) return toast('先填目标目录', false);
    const ok = await askConfirm(
      `把数据根从\n${R.root}\n迁到\n${t}\n\n`
      + `会做的事：整份复制过去（含 ${R.inbound.length} 个换号入口链接）→ 逐条目校验 → 改写账号档案里存的绝对路径 → 把 WorkBuddy / TRAE 等入口链接改指新位置 → 原位置改名留底不删。\n`
      + `前提：WorkBuddy / TRAE / ZCode / Qoder 这些程序得先全部退出，否则会被文件句柄挡住。\n`
      + `随时可以在这个页面点「退回原来的位置」还原。确认开始？`,
      { danger: false, okText: '开始迁移' });
    if (!ok) return;
    await run('storage_migrate', { target: t, confirm: true });
    loadStorage(true, true);
  };
  body.appendChild(panel('搬到别的盘',
    note('默认位置在家目录（C 盘）。指针文件只存一行路径，真身整体挪走，C 盘立刻腾出这块空间；换号机制照旧工作。'),
    row('目标目录', R.suggested ? `建议 ${sug} —— ${R.suggested.why}` : '填一个本地固定 NTFS 盘上的空目录',
      el('div', { style: 'display:flex;gap:8px;align-items:center' },
        target,
        actBtn('选文件夹', async () => {
          const r = await invoke('pick_folder', { current: target.value });
          if (r.ok) target.value = r.msg;
        }),
        btn('开始迁移', 'primary sm', go, '复制→校验→改路径→重挂入口→原位置留底')))));

  // 3) 迁移记录与回退
  const J = R.journal;
  if (J && J.steps) {
    const steps = (J.steps || []).map((s) => el('div', { class: 'invline' }, s));
    body.appendChild(panel('迁移记录',
      row('这一步走到哪了', `从 ${J.from} 到 ${J.to}，状态 ${J.status}`, null),
      ...steps,
      J.status === 'done' || J.status === 'switched'
        ? row('退回原来的位置', '把指针和入口链接改回旧位置（旧留底还在就能退）。要先退出所有 Agent 程序。',
          btn('退回原来的位置', 'sm', async () => {
            if (await askConfirm('退回迁移前的位置？\n新位置那份数据不会被删，确认旧位置能用之后再手动清理。', { danger: true, okText: '退回' })) {
              await run('storage_rollback', {});
              loadStorage(false);
            }
          }))
        : null));
  }

  // 4) 占用明细
  if (!R.sizes) {
    body.appendChild(panel('占了多少地方',
      note('下面这些数字要遍历账号目录才知道（WorkBuddy 一家就十几万个文件，约十几秒）。'),
      actBtn('重新统计', () => loadStorage(true, true))));
  } else {
    const itb = el('tbody');
    for (const a of (R.instances || [])) {
      const accs = (a.accounts || []).map((x) => `${x.name} ${mbText(x.mb)}`).join(' · ');
      itb.appendChild(el('tr', {},
        el('td', { class: 't' }, a.app),
        el('td', {}, mbText(a.mb)),
        el('td', {}, el('span', { class: 'kv' }, accs))));
    }
    body.appendChild(panel('占了多少地方',
      el('div', { class: 'invt' },
        `数据根合计 ${mbText(R.total_mb)} · 共享层 ${mbText(R.shared_mb)} · 备份 ${mbText(R.backups_mb)}`),
      el('div', { class: 'tablewrap' },
        el('table', { class: 'tbl' },
          el('thead', {}, el('tr', {},
            el('th', { class: 'col-app' }, '产品'),
            el('th', { style: 'width:16%' }, '实占'),
            el('th', {}, '各账号'))), itb)),
      row('多账号共用大文件', '新建账号时，程序自己下载的二进制 / 模型 / 虚拟机组件这类可再生大目录不再各存一份：收进 shared/ 只留一份，账号目录里挂一个链接过去。内容对不上（版本不同）就各留各的，如实说明。',
        btn('立即去重', 'sm', async () => {
          const r = await run('storage_dedup', {});
          if (r && r.ok) toast(r.msg, true);
          loadStorage(true, true);
        }))));
  }

  // 5) 入口链接与断链
  const dang = R.dangling || [];
  body.appendChild(panel('换号入口',
    note(`这些目录是指向数据根里账号目录的 junction（换号机制）：${R.inbound.map((p) => p.split('\\').pop()).join('、') || '（无）'}。它们占 0 字节，但删掉等于把程序的路切断。`),
    dang.length
      ? el('div', {}, dang.map((d) => el('div', { class: 'invline bad' },
        `断链：${d.link} → ${d.target}（目标不存在，程序会起不来；可在卡片上点「还原默认」或重跑迁移）`)))
      : el('div', { class: 'invline ok' }, '入口链接全部指向存在的位置。')));

  // 6) 垃圾体检
  const junk = S.junk || [];
  const jtb = el('tbody');
  for (const it of junk) {
    jtb.appendChild(el('tr', {},
      el('td', { class: 't', title: it.path }, it.path.split('\\').slice(-2).join('\\')),
      el('td', {}, mbText(it.mb)),
      el('td', {}, el('span', { class: 'kv' }, it.note)),
      el('td', {}, btn('删', 'danger sm', async () => {
        if (await askConfirm(`删掉这个？\n${it.path}\n${mbText(it.mb)}\n\n${it.note}`, { danger: true, okText: '删除' })) {
          await run('junk_clean', { ids: [it.id] });
          loadStorage(true, true);
        }
      }, '只删这一项'))));
  }
  const junkTotal = junk.reduce((s, x) => s + (x.mb || 0), 0);
  body.appendChild(panel(`可回收的残留（${junk.length} 项 · ${mbText(junkTotal)}）`,
    note('接管前的留底、恢复失败的残骸、断点下载的过期分片、两周以上的旧备份、配置历史备份。逐项列出来给你看，绝不自动删——只有明确点过的那几条才会动。'),
    junk.length
      ? el('div', { class: 'tablewrap' },
        el('table', { class: 'tbl' },
          el('thead', {}, el('tr', {},
            el('th', { class: 'col-app' }, '位置'),
            el('th', { style: 'width:12%' }, '大小'),
            el('th', {}, '是什么'),
            el('th', { style: 'width:10%' }, ''))), jtb))
      : el('div', { class: 'invline ok' }, '没扫出可回收的残留。'),
    actBtn('重新扫描', () => loadStorage(true, true))));
}

function securitySection(body, panel, row, actBtn) {
  const V = S.vault;
  if (!V) {
    if (!S.vaultBusy && !S.vaultError) loadVault();
    body.appendChild(panel('凭据保护',
      el('div', { class: 'empty' }, S.vaultBusy ? '正在读…'
        : (S.vaultError ? `没读到：${S.vaultError}` : '正在读…')),
      actBtn('刷新', () => loadVault())));
    return;
  }
  if (!V.supported) {
    body.appendChild(panel('凭据保护', el('div', { class: 'invline bad' }, '这台机器不是 Windows，没有 DPAPI，保管箱用不了。')));
    return;
  }
  const layers = V.has_password
    ? '两层：Windows 账户（DPAPI）+ 主密码（PBKDF2-SHA512 派生的附加熵）。必须同时有这台机器的这个账户、又输对主密码，才解得开。'
    : '一层：Windows 账户（DPAPI CurrentUser）。密文只绑定这台机器的你这个账户，程序自动解，界面无感；别人拷走文件或拆硬盘都读不出。';
  const p1 = pwCtrl('新主密码（至少 6 位）');
  const p2 = pwCtrl('再输一遍');
  const hint = el('input', { type: 'text', placeholder: '提示语（可选，忘了能看一眼）', style: 'width:200px' });
  const cur = pwCtrl('当前主密码');
  const unlockPwd = pwCtrl('主密码');

  body.appendChild(panel('保管箱',
    row('现在的保护层级', layers,
      el('span', { class: 'kv' }, V.has_password ? (V.locked ? '已设主密码 · 当前锁定' : '已设主密码 · 本次会话已解锁') : '仅 Windows 账户绑定')),
    row('已收多少凭据', `站点 API Key、AI 识别 Key、账号自带的 token，全部整包加密存进 vault.json；文档与界面里只剩打码串。`,
      el('span', { class: 'kv' }, `${V.count} 条`)),
    V.plaintext_left
      ? el('div', { class: 'invline warn' },
        `还有 ${V.plaintext_left} 处明文散在文档里（锁定期间保存的东西只能先原样放着）。解锁后会自动收进保管箱。`)
      : el('div', { class: 'invline ok' }, '文档里没有明文残留。'),
    V.error ? el('div', { class: 'invline bad' }, `保管箱解不开：${V.error}（一般是换了 Windows 账户；需要重新录一遍 Key）`) : null,
    row('文件位置', '密文就这一个文件，跟着数据根走。', el('div', { class: 'kv' }, V.file)),
    row('上次更新', V.updated || '（还没写过）', null)));

  if (V.locked) {
    body.appendChild(panel('解锁',
      note('锁着的时候界面看不到也复制不了任何 Key，反代和抓额度也用不了它们——这是设计，不是坏了。'),
      row('输入主密码', `提示：${V.hint || '（没设提示语）'}`,
        el('div', { style: 'display:flex;gap:8px;align-items:center' },
          unlockPwd,
          btn('解锁', 'primary sm', async () => {
            const r = await run('vault_unlock', { pwd: unlockPwd.value });
            if (r && r.ok) { unlockPwd.value = ''; loadVault(); }
          })))));
  }

  if (!V.has_password) {
    body.appendChild(panel('设主密码（第二层）',
      note('设了之后每次开机要输一次才用得了 Key；别人拿到这块硬盘、或复制走整个目录，都解不开。忘了密码没法找回，只能重录 Key。'),
      row('新主密码', '至少 6 位',
        el('div', { style: 'display:flex;gap:8px;align-items:center;flex-wrap:wrap' },
          p1, p2, hint,
          btn('设置', 'primary sm', async () => {
            const r = await run('vault_set_password', { pwd: p1.value, again: p2.value, hint: hint.value });
            if (r && r.ok) { p1.value = ''; p2.value = ''; loadVault(); }
          })))));
  } else {
    body.appendChild(panel('主密码',
      row('改主密码', '先解锁，再设新的（会用新随机盐重新派生并整包重加密）',
        el('div', { style: 'display:flex;gap:8px;align-items:center;flex-wrap:wrap' },
          p1, p2, hint,
          btn('改密码', 'sm', async () => {
            const r = await run('vault_set_password', { pwd: p1.value, again: p2.value, hint: hint.value });
            if (r && r.ok) { p1.value = ''; p2.value = ''; loadVault(); }
          }))),
      row('移除主密码', `退回只靠 Windows 账户这一层。当前有 ${V.count} 条凭据，移除后会重新加密一遍再落盘。`,
        btn('移除主密码', 'danger sm', async () => {
          if (await askConfirm('移除主密码？\n\n之后凭据只靠这台机器的 Windows 账户保护，开机即自动可读。', { danger: true, okText: '移除' })) {
            const r = await run('vault_clear_password', { confirm: true });
            if (r && r.ok) loadVault();
          }
        })),
      V.locked ? null : row('立即锁定', '清掉内存里的明文，下次要用得重新输密码。',
        btn('锁定', 'sm', async () => { await run('vault_lock', {}); loadVault(); }))));
  }

  body.appendChild(panel('这些不动',
    note('各 Agent 的登录态在它们自己的加密存储里（DPAPI / 自研方案），驾驶舱只搬运目录、不改内容；账号目录本身也没有加密——要防别人翻你磁盘，请把整个数据根放到有 BitLocker 的盘上。')));
}

function renderSettings() {
  const host = $('set-body').closest('.pagebody') || document.querySelector('.content');
  if (setLast !== setSection) {
    setLast = setSection;
    setScroll = 0;
  } else {
    setScroll = host.scrollTop;
  }
  document.querySelectorAll('#set-nav button').forEach((b) => {
    b.setAttribute('aria-current', String(b.dataset.sec === setSection));
  });
  const body = $('set-body');
  clear(body);
  const st = S.data.settings || {};
  const g = st.general || {}, a = st.appearance || {}, q = st.quota || {}, p = st.proxy || {}, pr = st.profile || {}, ai = st.ai || {};
  const panel = (title, ...rows) => el('section', { class: 'panel' }, el('h2', {}, title), ...rows);

  const nav = $('set-nav');
  if (!nav.firstChild) {
    for (const [id, label] of SET_SECTIONS) {
      const b = el('button', { type: 'button' }, label);
      b.dataset.sec = id;
      b.addEventListener('click', () => { setSection = id; renderSettings(); });
      nav.appendChild(el('li', {}, b));
    }
  }

  if (setSection === 'profile') {
    body.appendChild(panel('个人资料',
      row('昵称', '显示在窗口标题与备份文件名里，方便多机辨认是谁的配置',
        textCtrl(pr.nickname, '例如 任璐', (v) => setSetting('profile', 'nickname', v))),
      row('备注', '', textCtrl(pr.note, '这台机器的说明', (v) => setSetting('profile', 'note', v), true))));
  }

  if (setSection === 'general') {
    body.appendChild(panel('常规',
      row('开机自动启动', '登录后驻留托盘，继续监控额度与临期提醒',
        switchCtrl(g.autostart, async (v) => {
          const r = await invoke('set_autostart', { enable: v });
          toast(r.msg, r.ok);
          if (r.ok) setSetting('general', 'autostart', v);
        })),
      row('关闭窗口时', '托盘菜单里的「退出」始终可以彻底结束程序',
        segCtrl([['tray', '收进托盘'], ['exit', '直接退出']], g.close_action,
          (v) => setSetting('general', 'close_action', v))),
      row('临期额度弹系统通知', '启动后和每轮抓取后各检查一次，只在有临期时弹',
        switchCtrl(g.notify_alerts, (v) => setSetting('general', 'notify_alerts', v))),
      row('界面刷新间隔', '进程与额度状态的轮询频率，越小越跟手越费电',
        segCtrl([[2, '2 秒'], [4, '4 秒'], [10, '10 秒']], g.refresh_sec,
          (v) => setSetting('general', 'refresh_sec', v))),
      row('启动时自动扫描新装程序', '关掉后仍可手动点侧栏「重新扫描」',
        switchCtrl(g.auto_scan, (v) => setSetting('general', 'auto_scan', v)))));
    shellSection(body, panel, row, actBtn, btn);
  }

  if (setSection === 'appearance') {
    body.appendChild(panel('外观',
      row('主题', '深色石墨 + 琥珀金；浅色暖纸 + 深琥珀，两套都过了 WCAG AA 对比度',
        segCtrl([['light', '浅色'], ['system', '跟随系统'], ['dark', '深色']], a.theme,
          (v) => setSetting('appearance', 'theme', v))),
      row('界面密度', '紧凑模式缩小间距，一屏能看更多账号',
        segCtrl([['comfortable', '舒适'], ['compact', '紧凑']], a.density,
          (v) => setSetting('appearance', 'density', v))),
      row('字号', '',
        segCtrl([[85, '小'], [100, '标准'], [115, '大'], [130, '特大']], a.font_scale,
          (v) => setSetting('appearance', 'font_scale', v))),
      row('导航栏宽度', `${NAV_MIN}–${NAV_MAX} 像素；也可以直接拖动左栏与右栏之间的竖线，双击竖线恢复默认。名字被截断时就调宽这里。`,
        el('div', { class: 'ctlrow' },
          numCtrl(navWidth(a.nav_w), NAV_MIN, NAV_MAX, (v) => setSetting('appearance', 'nav_w', v)),
          actBtn('恢复默认', () => setSetting('appearance', 'nav_w', NAV_DEFAULT)))),
      row('左下角管理区高度', sideRatio(a)
        ? `现在是窗口高度的 ${Math.round(sideRatio(a) * 100)}%（约 ${ratioPx(sideRatio(a))} 像素），比它自己的内容矮时「管理」那一截自己滚动、上面的列表跟着变高。`
          + '存的是比例，所以窗口最大化 / 还原时它按同一个比例跟着变。拖动左栏中间那条横线即可调整，双击横线恢复自动。'
        : '自动：管理区正好装下它那几行（最多占窗口高度 22%，再矮的窗口里它自己滚），剩下的全给上面的列表。'
          + '横线往上拖到管理区显示完就到底，不会再拖出空白；往下拖是压缩它。',
        actBtn('恢复自动', () => commitSideHeight(0))),
      row('侧栏形态', '收成图标条后只剩一列程序图标，名字与徽章藏起来（悬停仍可看）。点左上角 Agents 旁的 « / » 或按 Ctrl+B 随时切换，切过一次就以你点的为准',
        segCtrl([['auto', '自动'], ['expand', '总是展开'], ['collapse', '总是收起']],
          a.nav_collapsed === true ? 'collapse' : (a.nav_collapsed === false ? 'expand' : 'auto'),
          (v) => setSetting('appearance', 'nav_collapsed', v === 'auto' ? null : v === 'collapse')))));
  }

  if (setSection === 'quota') {
    body.appendChild(panel('额度提醒',
      row('黄色提醒：过期前多少天', '顶栏的「临期 N」徽章里会出现该账号',
        numCtrl(q.warn_days, 1, 365, (v) => setSetting('quota', 'warn_days', v))),
      row('红色提醒：过期前多少天', '',
        numCtrl(q.danger_days, 1, 365, (v) => setSetting('quota', 'danger_days', v))),
      row('自动抓取间隔', '只对配了适配器或额度接口的账号生效',
        segCtrl([[10, '10 分钟'], [30, '30 分钟'], [60, '1 小时'], [240, '4 小时']], q.auto_fetch_min,
          (v) => setSetting('quota', 'auto_fetch_min', v)))));
  }

  if (setSection === 'proxy') {
    body.appendChild(panel('本地反代',
      row('监听端口', '只绑 127.0.0.1；改完需停止再启动反代才生效',
        numCtrl(p.port, 1024, 65535, (v) => setSetting('proxy', 'port', v))),
      row('默认上游', '', textCtrl(p.upstream, 'https://api.openai.com',
        (v) => setSetting('proxy', 'upstream', v), true)),
      row('请求日志保留条数', '',
        numCtrl(p.log_keep, 20, 1000, (v) => setSetting('proxy', 'log_keep', v)))));
  }

  if (setSection === 'ai') {
    body.appendChild(panel('多模态识别',
      row('Base URL', 'OpenAI 兼容接口地址，程序请求的是 <Base URL>/chat/completions。默认阿里云百炼的兼容模式端点，换成任意兼容接口都行',
        textCtrlId('ai-base-url', ai.base_url, 'https://dashscope.aliyuncs.com/compatible-mode/v1',
          (v) => setSetting('ai', 'base_url', v), true)),
      row('API Key', ai.key_held && !ai.key
        ? '已保存（原文在加密保管箱里，不回填到输入框）。要换新的就直接填，要清掉就留空并保存 —— 发请求时由程序自己拼进 Authorization 头，不经过界面'
        : '只存本机，落盘前进加密保管箱（vault.json）；发请求时由程序自己拼进 Authorization 头，不经过界面',
        textCtrlId('ai-key', ai.key, ai.key_held ? '（已保存，填了才覆盖）' : 'sk-…', (v) => setSetting('ai', 'key', v), true)),
      row('文本模型', '解析粘贴的 key 清单这类**纯文字**任务用，选快的小模型就够（qwen-flash / qwen3.5-flash 都行）。**别用思考模型**（qwen3 系会自动帮你关思考，但还是快模型更省时间）',
        textCtrl(ai.model, 'qwen-flash', (v) => setSetting('ai', 'model', v))),
      row('截图模型', '读积分页/活动页截图必须选**支持视觉**的模型。qwen-vl-flash 又快又便宜够日常用，vl-max 更准但慢几倍——识别慢先换它试试',
        textCtrl(ai.model_vision || '', 'qwen-vl-flash', (v) => setSetting('ai', 'model_vision', v))),
      row('获取模型列表', '向 <Base URL>/models 发一次请求，列出这个地址实际支持的模型；点列表里的名字直接填进「模型名」',
        el('div', { class: 'grow-wrap' }, aiModelsBtn())),
      aiModelsList(),
      row('连通性测试', '发一句「只回复 OK」，验证地址、Key、模型三者是否配对；结果留在下面这行，不是一闪而过的弹框',
        actBtn('测试', () => aiTest())),
      aiTestResult()));
    body.appendChild(panel('现在能识别什么',
      row('积分截图 → 逐笔额度', '卡片「账号设置 → 额度与用量 → 截图识别」，截图在剪贴板里时直接 Ctrl+V 也行',
        el('span', { class: 'kv' }, '已接')),
      row('活动截图 → 一条福利', '「福利活动 → ＋ 添加福利」表单里 Ctrl+V 贴截图，标题/说明/到期日/每天重复一起填好',
        el('span', { class: 'kv' }, '已接')),
      row('文字 / 截图 → API 台账', '「API 与额度」顶部「快速登记」：本机规则先读（离线不花 token），规则认不出 key 或地址才交给模型；认出来的站点、端点、key 与用途名当场建好',
        el('span', { class: 'kv' }, '已接')),
      row('登记完仍要逐条核对', '规则与模型都会认错，站点名、余额数字、过期日最容易出错。台账里点「改」随时修 —— 识别只负责把骨架搭出来，数字对不对还是你看',
        el('span', { class: 'kv' }, '落盘后可改'))));
  }

  // 新面板出错时当场显示原因，别把整页渲染带崩
  const safe = (fn, name) => {
    try {
      fn();
    } catch (err) {
      body.appendChild(el('section', { class: 'panel' },
        el('h2', {}, `${name}：渲染出错`),
        el('div', { class: 'invline bad' }, String((err && err.stack) || err))));
    }
  };
  if (setSection === 'storage') safe(() => storageSection(body, panel, row, actBtn), '存储');
  if (setSection === 'security') safe(() => securitySection(body, panel, row, actBtn), '凭据安全');

  if (setSection === 'advanced') {
    body.appendChild(panel('高级',
      row('重新扫描本机程序', '扫开始菜单与桌面快捷方式，收录新装的 AI 程序',
        actBtn('重新扫描', () => run('rescan'))),
      row('重置多开能力判定', '清掉实测记录，下次启动账号时重新判断该产品能否独立目录多开',
        actBtn('重置', () => run('reset_caps'))),
      row('打开数据目录', '账号档案、备份、设置都在这里',
        actBtn('打开', () => run('open_data_dir')))));

    const inv = S.inv;
    const invKids = [];
    if (inv) {
      invKids.push(el('div', { class: 'invt' },
        `本机认出 ${inv.detected} 家 · 目录收录 ${inv.catalog.loaded} 家 · `
        + (inv.first_run ? '第一次跑，只存基线不报增减' : `新装 ${inv.new.length} · 卸载 ${inv.gone.length}`)));
      if (inv.new.length) invKids.push(el('div', { class: 'invline ok' }, '新装：', inv.new.join('、')));
      if (inv.gone.length) {
        invKids.push(el('div', { class: 'invline warn' }, '这次没扫到（可能已卸载，档案与备份都还在）：', inv.gone.join('、')));
      }
      const stale = inv.stale_paths || [];
      if (!stale.length) {
        invKids.push(el('div', { class: 'invline ok' }, '内置清单里的可执行文件路径全部有效。'));
      }
      for (const s of stale) {
        const tail = s.found_at.length
          ? `；但痕迹里能找到 → ${s.found_at.join(' / ')}`
          : '；痕迹里也没有，多半是卸载了';
        invKids.push(el('div', { class: 'invline bad' },
          `${s.name}：内置路径 ${s.recorded.join(' 或 ')} 指不到文件${tail}`));
      }
    } else {
      invKids.push(el('div', { class: 'empty' }, '还没盘点过，点上方「扫描登记」跑一次。'));
    }
    body.appendChild(el('section', { class: 'panel' },
      el('h2', {}, '本机 Agent 盘点'),
      el('p', { style: 'font-size:12.5px;color:var(--text-muted);margin-bottom:10px' },
        '点卡片区上方的「扫描登记」跑一次：三路查安装痕迹（注册表卸载表 / 开始菜单与桌面快捷方式 / 安装根目录与 npm 全局），'
        + '与上次快照比对，报出新装、卸载，以及内置清单里已经指不到文件的路径。'
        + '预存的各家索引信息在 ~/.agents/catalog.json（由 agenthub catalog build 生成）。'),
      ...invKids));

    const bs = S.data.backups || [];
    const totalMb = bs.reduce((s, b) => s + (b.mb || 0), 0);
    const btb = el('tbody');
    for (const b of bs) {
      btb.appendChild(el('tr', {},
        el('td', { class: 't', title: b.file }, `${b.app} / ${b.account}`),
        el('td', {}, `${b.mb} MB`),
        el('td', {}, b.at),
        el('td', {}, btn('删除', 'danger sm', async () => {
          if (await askConfirm(`删除备份 ${b.file}？\n删掉后这个时间点的快照就没了（账号本身不受影响）。`,
            { danger: true, okText: '删除备份' })) {
            run('delete_backup', { file: b.file });
          }
        }, '只删这份 zip 快照，不影响账号目录'))));
    }
    body.appendChild(el('section', { class: 'panel' },
      el('h2', {}, `账号备份（${bs.length} 份 · 共 ${totalMb.toFixed(1)} MB）`),
      el('p', { style: 'font-size:12.5px;color:var(--text-muted);margin-bottom:10px' },
        '备份是把某个账号的整个数据目录打包成 zip 存到数据根的 backups/ 下（位置见 设置→存储），'
        + '会真实占用磁盘（已自动跳过 Cache/logs 等可再生缓存）。删掉备份不影响账号本身，只丢这份快照。'),
      bs.length
        ? el('div', { class: 'tablewrap' },
          el('table', { class: 'tbl' },
            el('thead', {}, el('tr', {},
              el('th', { class: 'col-app' }, '账号'),
              el('th', { style: 'width:15%' }, '大小'),
              el('th', { style: 'width:22%' }, '时间'),
              el('th', { style: 'width:15%' }, ''))), btb))
        : el('div', { class: 'empty' }, '还没有备份。在账号卡片上点「备份」生成第一份。')));

    body.appendChild(el('section', { class: 'panel' },
      el('h2', {}, '手动添加程序'),
      row('登记一个本机程序', '自动扫描认不出来的（绿色版、没建快捷方式、名字里不含 AI 关键词）用它：只需可执行文件路径，不改动该程序的任何文件；能不能多开由内核实测判定（同目录有 resources.pak/locales 就认 Chromium）。侧栏下方也有同一个入口',
        actBtn('＋ 添加程序', openAddApp))));

    const hid = S.data.hidden_apps || [];
    // 登记表里那批"官方默认路径"的产品（Antigravity / Cursor / Kiro / Windsurf / Zed /
    // GitHub Copilot / Grok / CodeBuddy / Trae 系）默认只在装了、或已有账号卡时才占侧栏一格。
    // 这个开关把它们全放出来——要看全量登记表、或者想给还没装的先建卡时用。
    const showAll = !!(S.data.show_uninstalled);
    body.appendChild(el('section', { class: 'panel' },
      el('h2', {}, '未安装的程序'),
      row('显示未安装的程序',
        '登记表里预置了十几个 Agent 的官方默认安装位置，换台装了它们的机器就能自动认出来。'
        + '默认这些"这台机器上没装、也没有账号卡"的条目不出现在左侧列表里——否则光是没装的就会多出十几条。'
        + `现在：${showAll ? '全部显示' : '只显示装了或有账号卡的'}。`,
        actBtn(showAll ? '收起未安装的' : '全部显示', async () => {
          await run('show_uninstalled', { on: !showAll });
        }))));

    const htb = el('tbody');
    for (const h of hid) {
      htb.appendChild(el('tr', {},
        el('td', { class: 't', title: h.exe }, h.name),
        el('td', {}, chip(h.discovered ? '自动发现' : '内置', '')),
        el('td', {}, btn('恢复显示', 'sm', () => run('unhide_app', { app: h.id }), '让它重新出现在左侧列表'))));
    }
    body.appendChild(el('section', { class: 'panel' },
      el('h2', {}, `已隐藏的程序（${hid.length}）`),
      el('p', { style: 'font-size:12.5px;color:var(--text-muted);margin-bottom:10px' },
        '这里列出的是已经被隐藏掉的程序，点「恢复显示」把它放回左侧列表。'
        + '想隐藏某个程序：在左侧程序名上右键选「隐藏程序」，或选中它后点卡片区右上的「隐藏程序」。隐藏不会卸载程序，也不动它的账号目录。'),
      hid.length
        ? el('div', { class: 'tablewrap' },
          el('table', { class: 'tbl' },
            el('thead', {}, el('tr', {},
              el('th', { class: 'col-app' }, '程序'),
              el('th', { style: 'width:20%' }, '来源'),
              el('th', { style: 'width:22%' }, ''))), htb))
        : el('div', { class: 'empty' }, '还没有隐藏任何程序。不想看到某个程序，就在左侧右键它 →「隐藏程序」。')));
  }

  if (setSection === 'help') safe(() => renderHelp(body, panel), '帮助与教程');

  if (setSection === 'about') {
    body.appendChild(panel('关于',
      row('版本', 'Tauri 2 + Rust 原生壳（WebView2），与 Cockpit Tools 同构',
        el('span', { class: 'kv' }, `v${S.data.app_version || '2.0.0'}`)),
      row('数据位置', '全部只存本机，不随代码上传。要迁到别的盘（或看占了多少地方）去 设置→存储',
        el('span', { class: 'kv' }, S.data.data_root || '~/.agent-cockpit/')),
      row('Key 保护', S.data.vault && S.data.vault.has_password
        ? `加密保管箱 ${S.data.vault.count} 条 · Windows 账户 + 主密码${S.data.vault.locked ? ' · 当前锁定' : ''}`
        : `加密保管箱 ${S.data.vault ? S.data.vault.count : 0} 条 · Windows 账户绑定（DPAPI），可在 设置→凭据安全 加一层主密码`,
        actBtn('去设置', () => { setSection = 'security'; renderSettings(); })),
      row('仓库', '私有仓库 github.com/shen1950/agent-cockpit',
        actBtn('复制地址', () => copyText('https://github.com/shen1950/agent-cockpit'))),
      row('监听地址', '反代与界面只绑回环，不对外网开放',
        el('span', { class: 'kv' }, `127.0.0.1:${p.port || 8788}`))));
  }
  host.scrollTop = setScroll;
}
// 本机余额直读：串号时先问一句要不要改绑，绝不静默覆盖另一个账号的数
/* 读数结果的三种收尾：认人（改绑）、缺登录（开这个号自己的窗口）、成功（报一句）。
   pullLocal 和 refreshQuota 共用，避免两条路各写一遍、说法还不一样。 */
const readDone = (app, name, r) => {
  toast(r.msg, r.ok);
  if (!r.ok) return;
  // 后端可能顺手把「账号N」改成真实用户名：浮窗开着的话得跟上改名，否则下次点击用旧名找不到实例
  if (r.renamed && S.detail && S.detail.app === app && S.detail.name === name) S.detail.name = r.renamed;
  load();
};
async function rebindAndRetry(app, name, r, act) {
  const f = r.found || {};
  const again = await askConfirm(
    `${r.msg}

本机当前登录：${f.account || '?'}（剩 ${nfmt(f.left)} / 共 ${nfmt(f.total)}，到期 ${dtxt(f.expires_on)}）`,
    { title: r.already_bound ? '这个登录态已经绑在别的账号上了' : '本机登录的账号和这个档案不是同一个',
      danger: true, okText: r.already_bound ? '仍然绑定为同一个号' : '改绑为当前账号' });
  if (!again) return;
  readDone(app, name, await invoke('action', { payload: { action: act, app, name, force: true } }));
}
async function pullLocal(app, name, act = 'pull_local') {
  const r = await invoke('action', { payload: { action: act, app, name } })
    .catch((e) => ({ ok: false, msg: `读取失败：${e}` }));
  if (r.mismatch || r.already_bound) return rebindAndRetry(app, name, r, act);
  readDone(app, name, r);
  return r;
}
/* 卡片上唯一的读数入口。后端 quota_refresh 自己挑层：
   官网逐包（用量 + 每个包 + 各自到期 + 每日消耗）→ 读不到才退到本机/服务端汇总。
   缺会话时不再让她去找第二颗按钮——当场把这个号自己的登录窗口开起来。 */
async function refreshQuota(app, name) {
  toast(`正在读「${name}」的额度…`, true);
  const r = await invoke('action', { payload: { action: 'quota_refresh', app, name } })
    .catch((e) => ({ ok: false, msg: `读取失败：${e}` }));
  if (r.mismatch || r.already_bound) return rebindAndRetry(app, name, r, 'quota_refresh');
  if (r.wrong_account) {
    const go = await askConfirm(r.msg, { title: '这个号的官网会话登错了人', okText: '打开它的登录窗口' });
    if (go) await webLogin(app, name);
    return r;
  }
  readDone(app, name, r);
  if (r.need_login) {
    const go = await askConfirm(
      `${r.detail_error || '这个号还没登过官网'}\n\n现在打开「${name}」自己的官网登录窗口？`
      + '登录一次，之后每次刷新都直接读这个号的逐包额度和到期时间，跟本机客户端登的是谁无关。',
      { title: '逐包额度和到期时间要登录一次', okText: '打开登录窗口' });
    if (go) await webLogin(app, name);
  }
  return r;
}
/* ---------------- 官网逐资源包额度 ----------------
   本机接口只给两个汇总数，每个资源包各自的到期日只有官网用量页有。
   登录走驾驶舱自己的 WebView2 窗口（会话按账号存在数据根 webprofile/<产品>/<这个号>/），
   取数由 Rust 侧带着那份会话直接调接口。 */
async function webLogin(app, name, uid) {
  const r = await invoke('action', { payload: { action: 'web_login', app, name: name || '', user_id: uid || '' } })
    .catch((e) => ({ ok: false, msg: String(e) }));
  toast((r && r.msg) || '打不开官网登录窗口', !!(r && r.ok));
  if (r && r.ok) afterWebLogin(app, name || '', uid || '');
}
/* 官网窗口里一登录成功就自动补一次取数。她的预期是"我登进去了，积分、用量、到期时间
   就该都在"，不该还要再点第二个按钮——早先 wzWebLogin 只设了个 webWaiting 标记，没人读它。
   轮询走 web_session：只翻那个窗口自己的 cookie 罐、不发请求，认到会话才真去取数。 */
let webPoll = null;
async function afterWebLogin(app, name, uid) {
  if (webPoll) return;
  if (!name && !S.wizard) return;
  webPoll = true;
  const t0 = Date.now();
  try {
    while (Date.now() - t0 < 6 * 60 * 1000) {
      await new Promise((r) => setTimeout(r, 4000));
      const s = await invoke('action', { payload: { action: 'web_session', app, name: name || '', user_id: uid || '' } })
        .catch(() => null);
      if (!s || !s.signed_in) continue;
      toast('官网登录上了，正在自动读逐包额度…', true);
      if (name) await refreshQuota(app, name);
      else await wzLoadQuota(true);
      return;
    }
    toast('官网窗口里一直没检测到登录会话：登好了再点一次卡片上的「刷新额度」。', false);
  } finally {
    webPoll = null;
  }
}
async function copyText(t) {
  try { await navigator.clipboard.writeText(t); toast('已复制到剪贴板'); }
  catch (e) { toast(`复制失败：${e}`, false); }
}

/* ---------------- 贴图识别（多模态 AI，OpenAI 兼容接口） ----------------
   国产 Agent 的积分页/活动页既没接口也没法自动抓，只能截图 → 让模型读成结构化数据。
   提示词一律要求"照原样、看不清留空、不要编造"，回填后由用户自己核对再保存。 */
const AI_PROMPT_QUOTA = '这是某个 AI 产品的用量/积分截图。只输出一个 JSON 本体：不要 markdown 代码块围栏，不要任何解释文字。\n'
  + '{"credits":[{"label":"这笔积分的名称","category":"截图上这一组的大标题（如 套餐基础积分 / 平台奖励积分 / 购买积分，没有就写其他）",'
  + '"left":剩余额度数字,"total":总额度数字,"expires_on":"YYYY-MM-DD HH:MM，图上只到日就写日期，没有或看不清就空字符串"}],'
  + '"note":"截图里其它与额度有关的一句话说明，没有就空字符串"}\n'
  + '规则：数字照截图原样，不要换算、不要四舍五入、不要编造；日期照图上的时间原样抄，看不清就留空；只有一笔也要放进 credits 数组。';
const AI_PROMPT_PERK = '这是某个 AI 产品的限时福利/免费活动截图。只输出一个 JSON 本体：不要 markdown 代码块围栏，不要任何解释文字。\n'
  + '{"title":"一句话活动名","detail":"规则说明（倍率、赠送额度、重置时间等）",'
  + '"ends_on":"YYYY-MM-DD，长期或没写就空字符串","daily":true 或 false（是否每天重复）}\n'
  + '看不清的字段留空字符串，不要编造日期。';

function fileToDataUrl(f) {
  return new Promise((res, rej) => {
    const rd = new FileReader();
    rd.onload = () => res(rd.result);
    rd.onerror = () => rej(new Error('读不了这个文件'));
    rd.readAsDataURL(f);
  });
}

async function aiRecognize(kind, dataUrl) {
  if (AI_BUSY.on) { aiBusyRefuse(); return; }
  aiProgressShow(kind === 'perk' ? 'AI 读活动截图' : 'AI 读积分截图');
  let r;
  try {
    r = await invoke('action', {
      payload: { action: 'ai_ask', prompt: kind === 'perk' ? AI_PROMPT_PERK : AI_PROMPT_QUOTA, image: dataUrl },
    });
  } catch (e) {
    toast(`调用失败：${e}`, false);
    return;
  } finally {
    aiProgressHide();
  }
  if (!r.ok) { toast(r.msg, false); return; }
  const j = r.json;
  if (!j) { toast('模型没给出 JSON，原样贴在下面自己抄：\n' + r.text, false); return; }
  if (kind === 'perk') {
    S.perkForm = { item: null, prefill: {
      app: S.cur || '', title: j.title || '', detail: j.detail || '',
      ends_on: j.ends_on || '', daily: !!j.daily, url: '',
    } };
    renderPerks(S.data);
    toast('识别好了，核对一下再保存', true);
    return;
  }
  const list = Array.isArray(j.credits) ? j.credits : [];
  if (!S.detail && !S.wizard) { toast('账号设置或建号向导没打开，认出来的积分没地方放', false); return; }
  previewQuotaCredits(list, j.note || '');
}

/* 截图识别出来的逐笔积分：界面上没有手填框，认完弹一层预览，核对过才落盘
   （账号设置里 → 写进这张卡；建号向导里 → 带进向导，随卡片一起建）。 */
let pendingQuota = null;
function previewQuotaCredits(list, note) {
  const toWizard = !!S.wizard && !S.detail;
  qdetail = null; S.qdetail = null;
  // 模型偶尔把同一笔积分读出来两遍：先按「名称+剩+总+到期」去重再摆，
  // 内容完全一样才并——两笔真的同额同名同到期，留着也只是重复行。
  const seen = new Set();
  list = list.filter((c) => {
    const sig = [c.label, c.left, c.total, c.expires_on].join('|');
    if (seen.has(sig)) return false;
    seen.add(sig);
    return true;
  });
  pendingQuota = list.length ? { list, toWizard } : null;
  $('qmodal').hidden = false;
  $('qm-title').textContent = '截图识别结果 · 核对后保存';
  clear($('qm-tabs'));
  $('qm-sum').textContent = list.length
    ? `共 ${list.length} 笔 · 合计剩 ${nfmt(list.reduce((s, c) => s + (Number(c.left) || 0), 0))}`
      + ` / ${nfmt(list.reduce((s, c) => s + (Number(c.total) || 0), 0))}`
      + (note ? ` · ${note}` : '')
    : '没识别出逐笔积分，可能是截图里没有额度数字';
  const box = $('qm-list');
  clear(box);
  for (const c of list) {
    const t = Number(c.total) || 0;
    const l = Number(c.left) || 0;
    const p = t ? Math.max(0, Math.min(100, (l / t) * 100)) : 0;
    box.appendChild(el('div', { class: 'qitem' },
      el('div', { class: 'qitop' }, el('b', {}, c.label || '未命名'),
        c.category ? el('span', { class: 'qtag' }, c.category) : null),
      el('div', { class: 'qimid' },
        el('span', {}, `到期时间 ${dtxt(c.expires_on)}`),
        el('span', { class: 'qistat' }, `剩 ${nfmt(l)} / 共 ${nfmt(t)}`)),
      el('div', { class: 'cbar' }, el('b', { style: `width:${p}%` }))));
  }
  $('qm-save').hidden = !list.length;
  $('qm-save').textContent = pendingQuota && pendingQuota.toWizard ? '用这几笔' : '核对无误，保存';
  toast(list.length ? `识别出 ${list.length} 笔，核对后点保存` : '没识别出逐笔积分，可能是截图里没有额度数字', !!list.length);
}
async function savePendingQuota() {
  if (!pendingQuota) return;
  const { list, toWizard } = pendingQuota;
  if (toWizard && S.wizard) {
    S.wizard.credits = list.map((c) => ({
      label: c.label || '', left: c.left ?? null, total: c.total ?? null,
      expires_on: c.expires_on || '',
    }));
    pendingQuota = null;
    $('qm-save').hidden = true;
    $('qmodal').hidden = true;
    renderWizard();
    return;
  }
  if (!S.detail) return;
  const { app, name } = S.detail;
  await run('save_meta', {
    app, name,
    credits: list.map((c) => ({
      label: c.label || '', left: c.left ?? '', total: c.total ?? '',
      category: c.category || '', unit: c.unit || 'credits',
      expires_on: c.expires_on || '', note: c.note || '',
    })),
  });
  pendingQuota = null;
  $('qm-save').hidden = true;
  $('qmodal').hidden = true;
}

// 截图 → 直接 Ctrl+V 贴进当前打开的表单；留一个选文件的兜底口子。
// 别用原生 file input：它自带 ~250px 的"选择文件 未选择文件"，在 .setrow 的 auto 列里
// 会把左边的说明挤成一列一个字。
function pasteHint(kind) {
  const pick = el('input', { type: 'file', accept: 'image/*', class: 'filepick', hidden: true });
  pick.addEventListener('change', () => {
    const f = pick.files && pick.files[0];
    if (f) fileToDataUrl(f).then((u) => aiRecognize(kind, u)).catch((e) => toast(e.message, false));
    pick.value = '';
  });
  return el('div', { class: 'aihint' },
    el('span', { class: 'kv' }, '或'),
    btn('选图片…', 'sm', () => pick.click(), '从文件里挑一张截图；截图在剪贴板里时直接 Ctrl+V 更快'),
    pick);
}

function bindAiPaste() {
  document.addEventListener('paste', async (e) => {
    const items = e.clipboardData && e.clipboardData.items;
    if (!items) return;
    let file = null;
    for (const it of items) {
      if (it.type && it.type.startsWith('image/')) { file = it.getAsFile(); break; }
    }
    if (!file) return;   // 没有图片就交给原来的文本粘贴逻辑
    let kind = null;
    if (S.perkForm) kind = 'perk';
    else if (!$('imodal').hidden && S.detail && S.detail.sec === 'quota') kind = 'quota';
    else if (!$('wizard').hidden && S.wizard && WZ_STEPS[S.wizard.step].id === 'quota') kind = 'quota';
    else if (S.view === 'keys') kind = 'api';   // API 页贴截图 = 登记这个站
    if (!kind) { toast('先在「账号设置 → 额度与用量」或建号向导的额度那一步贴截图', false); return; }
    e.preventDefault();
    try {
      const url = await fileToDataUrl(file);
      if (kind === 'api') {
        // key 弹窗里已经没有粘贴区了：贴截图一律走页顶「快速登记」，别越过弹窗直接建站点
        if (!$('kmodal').hidden) { toast('要登记新站先关掉这个弹窗，贴到页顶的「快速登记」', false); return; }
        intakeRegister(url);
      } else {
        aiRecognize(kind, url);
      }
    } catch (err) {
      toast(`读剪贴板图片失败：${err.message}`, false);
    }
  });
}


/* 连通性测试：结果常驻在「测试」按钮下面那一行。
   光靠 toast 等于没提示——5 秒就没了，成功失败都看不见。 */
let aiTestState = null;
function aiTestResult() {
  const s = aiTestState;
  if (!s) return el('div');
  if (s.busy) return el('div', { class: 'hintline' }, `正在发请求（${s.target}）…`);
  return el('div', { class: `hintline${s.ok ? '' : ' bad'}` }, `${s.ok ? '通了' : '没通'} · ${s.at} · ${s.msg}`);
}

async function aiTest() {
  // 连通性测试发的是纯文本，验的就是「文本模型」那一档
  const model = S.data?.settings?.ai?.model || 'qwen-flash';
  const at = () => new Date().toLocaleTimeString('zh-CN', { hour12: false });
  aiTestState = { busy: true, ok: false, at: at(), target: model, msg: '' };
  renderSettings();
  let r = null;
  try {
    r = await invoke('action', {
      payload: { action: 'ai_ask', prompt: '只回复两个字：OK', image: '' },
    });
  } catch (e) {
    r = { ok: false, msg: `调用失败：${e}` };
  }
  const ans = String(r.text ?? '').trim();
  aiTestState = {
    busy: false,
    ok: !!r.ok,
    at: at(),
    msg: r.ok
      ? `模型答：「${ans.slice(0, 40) || '（空回复）'}」，地址 / Key / 模型三样都对得上（${model}）`
      : r.msg,
  };
  toast(r.ok ? `接口可用：${ans.slice(0, 24) || '（空回复）'}` : r.msg, r.ok);
  renderSettings();
}

/* 获取模型列表：base/key 优先取输入框里还没落盘的现值（change 触发前 S.data 里是旧值） */
let aiModelsBusy = false;
function aiModelsBtn() {
  const b = actBtn(aiModelsBusy ? '拉取中…' : '获取', async () => {
    if (aiModelsBusy) return;
    const base = aiSettingInput('base_url');
    const key = aiSettingInput('key');
    aiModelsBusy = true;
    S.aiModels = { state: 'loading', list: [], at: '' };
    renderSettings();
    const r = await invoke('action', { payload: { action: 'ai_list_models', base, key } });
    aiModelsBusy = false;
    if (!r.ok) {
      S.aiModels = { state: 'fail', list: [], at: '', msg: r.msg };
      toast(`拉不到模型：${r.msg}`, false);
    } else {
      S.aiModels = { state: 'ok', list: r.models || [], at: new Date().toLocaleTimeString() };
      toast(r.msg, true);
    }
    renderSettings();
  });
  b.disabled = aiModelsBusy;
  return b;
}
function aiSettingInput(key) {
  const map = { base_url: 'ai-base-url', key: 'ai-key' };
  const inp = document.getElementById(map[key]);
  if (inp && inp.value.trim()) return inp.value.trim();
  const ai = S.data?.settings?.ai || {};
  return key === 'key' ? (ai.key || '') : (ai.base_url || '');
}
function aiModelsList() {
  const m = S.aiModels;
  if (!m || m.state === 'idle') return el('div');
  if (m.state === 'loading') {
    return el('div', { class: 'hintline' }, '正在向上游要模型清单…');
  }
  if (m.state === 'fail') {
    return el('div', { class: 'hintline bad' }, `拉取失败：${m.msg}`);
  }
  const cur = S.data?.settings?.ai?.model || '';
  const wrap = el('div', { class: 'modelpick' },
    el('div', { class: 'flabel' }, `上游声明支持 ${m.list.length} 个模型（${m.at}）——点名字填进「模型名」`));
  const chips = el('div', { class: 'chips' });
  for (const id of m.list) {
    const c = el('button', { type: 'button', class: `chip${id === cur ? ' on' : ''}`, title: id }, id);
    c.addEventListener('click', () => setSetting('ai', 'model', id));
    chips.appendChild(c);
  }
  wrap.appendChild(chips);
  return wrap;
}

/* ---------------- 自定义右键菜单（替掉浏览器那套） ---------------- */
function ctxItem(label, key, fn) {
  const b = el('button', { type: 'button' }, label, key ? el('span', {}, key) : null);
  b.addEventListener('click', () => { hideCtx(); fn(); });
  return b;
}
function hideCtx() { $('ctx').hidden = true; clear($('ctx')); }
function showCtx(x, y, items) {
  const m = $('ctx');
  clear(m);
  for (const it of items) {
    if (it === '-') { m.appendChild(el('hr')); continue; }
    m.appendChild(it);
  }
  m.hidden = false;
  const r = m.getBoundingClientRect();
  m.style.left = `${Math.min(x, window.innerWidth - r.width - 8)}px`;
  m.style.top = `${Math.min(y, window.innerHeight - r.height - 8)}px`;
}
function bindCtx() {
  document.addEventListener('contextmenu', (e) => {
    e.preventDefault();
    hideCtx();
    const input = e.target.closest('input, textarea');
    const sel = String(window.getSelection() || '').trim();
    const items = [];
    if (input) {
      items.push(
        ctxItem('剪切', 'Ctrl+X', async () => {
          const v = input.value;
          const a = input.selectionStart, b = input.selectionEnd;
          if (b > a) { copyText(v.slice(a, b)); input.value = v.slice(0, a) + v.slice(b); input.dispatchEvent(new Event('change')); }
        }),
        ctxItem('复制', 'Ctrl+C', () => {
          const v = input.value;
          const a = input.selectionStart, b = input.selectionEnd;
          copyText(b > a ? v.slice(a, b) : v);
        }),
        ctxItem('粘贴', 'Ctrl+V', async () => {
          try {
            const t = await navigator.clipboard.readText();
            const a = input.selectionStart ?? input.value.length;
            input.value = input.value.slice(0, a) + t + input.value.slice(input.selectionEnd ?? a);
            input.dispatchEvent(new Event('change'));
          } catch { toast('系统剪贴板读取未授权，请用 Ctrl+V', false); }
        }),
        ctxItem('全选', 'Ctrl+A', () => input.select()));
    } else {
      if (sel) items.push(ctxItem('复制选中', 'Ctrl+C', () => copyText(sel)));
      const navBtn = e.target.closest('#applist button[data-app]');
      if (navBtn) {
        const id = navBtn.dataset.app;
        const a = (S.data?.apps || []).find((x) => x.id === id);
        items.push(
          ctxItem('打开这个程序', '', () => run('launch_app', { app: id })),
          ctxItem('置顶', '', () => run('app_move', { app: id, op: 'top' })),
          ctxItem('上移一位', '', () => run('app_move', { app: id, op: 'up' })),
          ctxItem('下移一位', '', () => run('app_move', { app: id, op: 'down' })),
          ctxItem('隐藏程序', '', async () => {
            if (a && await askConfirm(`把「${a.name}」从列表隐藏？\n不会卸载程序，也不动它的账号目录；可在 设置 → 高级 里恢复。`,
              { okText: '隐藏' })) {
              run('hide_app', { app: id });
            }
          }),
          ...(a && a.custom ? [ctxItem('从列表移除', '', async () => {
            if (await askConfirm(`移除手动添加的「${a.name}」？\n只是不再显示在列表里，程序本身和它的账号目录都不动。`,
              { danger: true, okText: '移除' })) {
              const r = await invoke('remove_app', { app: id });
              toast(r.msg, r.ok);
              load();
            }
          })] : []),
          ...(a && a.removed ? [ctxItem('清理记录', '', async () => {
            if (await askConfirm(`移除「${a.name}」的卸载记录？\n账号目录原样保留，程序已卸载无需再显示。`,
              { danger: true, okText: '清理' })) {
              run('forget_app', { app: id });
            }
          })] : []),
          '-');      }
      items.push(
        ctxItem('刷新', 'Ctrl+R', load),
        ctxItem('设置', '', () => { S.view = 'settings'; render(); }),
        '-',
        ctxItem('重新扫描程序', '', () => run('rescan')));
    }
    showCtx(e.clientX, e.clientY, items);
  });
  document.addEventListener('click', (e) => { if (!e.target.closest('#ctx')) hideCtx(); });
  window.addEventListener('blur', hideCtx);
  document.addEventListener('keydown', (e) => { if (e.key === 'Escape') hideCtx(); });
}

/* ---------------- 侧栏宽度：拖动 / 双击复位 / 方向键微调 ---------------- */
function bindNavResize() {
  const h = $('nav-resize');
  const zoom = () => (Number(S.data?.settings?.appearance?.font_scale) || 100) / 100;
  const store = () => navWidth(S.data?.settings?.appearance?.nav_w);
  const apply = (px) => document.documentElement.style.setProperty('--nav-w-user', `${px}px`);
  const commit = (px) => {
    apply(px);
    if (px !== store()) setSetting('appearance', 'nav_w', px);
    else toast(`导航栏宽度 ${px}px`);
  };

  let from = 0, base = 0;
  h.addEventListener('pointerdown', (e) => {
    e.preventDefault();
    navDrag.active = true;
    from = e.clientX;
    base = store();
    h.classList.add('dragging');
    h.setPointerCapture(e.pointerId);
  });
  h.addEventListener('pointermove', (e) => {
    if (!navDrag.active) return;
    apply(navWidth(base + (e.clientX - from) / zoom()));
  });
  h.addEventListener('pointerup', (e) => {
    if (!navDrag.active) return;
    navDrag.active = false;
    h.classList.remove('dragging');
    commit(navWidth(base + (e.clientX - from) / zoom()));
  });
  h.addEventListener('dblclick', () => commit(NAV_DEFAULT));
  h.addEventListener('keydown', (e) => {
    const step = e.key === 'ArrowRight' ? 16 : e.key === 'ArrowLeft' ? -16 : 0;
    if (!step) return;
    e.preventDefault();
    commit(navWidth(store() + step));
  });
}

/* ---------------- 侧栏管理区的高度：拖那条横线 / 双击复位 / 方向键微调 ---------------- */
function bindSideResize() {
  const h = $('side-resize');
  const sb = h.closest('.sidebar');
  const top = sb.querySelector('.side-scroll');
  const fx = sb.querySelector('.side-fixed');
  const zoom = () => (Number(S.data?.settings?.appearance?.font_scale) || 100) / 100;
  const store = () => ratioPx(sideRatio(S.data?.settings?.appearance || {}));
  const contentH = (node) => {
    const last = node.lastElementChild;
    if (!last) return 0;
    return Math.ceil((last.getBoundingClientRect().bottom
      - node.getBoundingClientRect().top + node.scrollTop) / zoom());
  };
  // 只有列表区和管理区参与分配，其余兄弟（顶栏 + 这条手柄）连外边距一起扣掉
  const avail = () => {
    let chrome = 0;
    for (const kid of sb.children) {
      if (kid === top || kid === fx) continue;
      const cs = getComputedStyle(kid);
      chrome += kid.getBoundingClientRect().height
        + parseFloat(cs.marginTop || 0) + parseFloat(cs.marginBottom || 0);
    }
    return sb.getBoundingClientRect().height / zoom() - chrome / zoom();
  };
  // 指针越靠下 = 管理区越矮；以列表区的顶边为原点（它不随拖动移动）
  const at = (y) => Math.round(avail() - (y - top.getBoundingClientRect().top) / zoom());
  // 上限就是管理区的内容高——再往上拖多出来的空间没人接，就是左下角那片空白
  const clamp = (px) => {
    const hi = Math.max(SIDE_MIN, Math.min(contentH(fx), avail() - SIDE_MIN));
    return Math.round(Math.min(Math.max(px, SIDE_MIN), hi));
  };
  const shown = () => store() || Math.round(fx.getBoundingClientRect().height / zoom());

  h.addEventListener('pointerdown', (e) => {
    if (document.documentElement.classList.contains('rail')) return;   // 图标条模式下这两截没得分配
    e.preventDefault();
    sideDrag.active = true;
    h.classList.add('dragging');
    applySideHeight(clamp(at(e.clientY)));   // 先给反馈，再要指针捕获
    h.setPointerCapture(e.pointerId);
  });
  h.addEventListener('pointermove', (e) => {
    if (sideDrag.active) applySideHeight(clamp(at(e.clientY)));
  });
  h.addEventListener('pointerup', (e) => {
    if (!sideDrag.active) return;
    sideDrag.active = false;
    h.classList.remove('dragging');
    commitSideHeight(clamp(at(e.clientY)));
  });
  h.addEventListener('pointercancel', () => {
    if (!sideDrag.active) return;
    sideDrag.active = false;
    h.classList.remove('dragging');
    applySideHeight(store());
  });
  h.addEventListener('dblclick', () => commitSideHeight(0));
  h.addEventListener('keydown', (e) => {
    const step = e.key === 'ArrowDown' ? 16 : e.key === 'ArrowUp' ? -16 : 0;
    if (!step) return;
    e.preventDefault();
    commitSideHeight(clamp(shown() + step));
  });
}

/* ---------------- 交互 ---------------- */
async function swapSet(app, name) {
  const i = S.data.instances.find((x) => x.app === app && x.name === name);
  // 切到空目录会让目标程序变成未登录/空白（WorkBuddy 直接黑屏），先确认再动
  if (i && i.identity && i.identity.state === 'none') {
    if (!await askConfirm(`「${name}」的账号目录还是空的。\n\n换号后 ${i.app_name} 打开会是未登录或空白界面（有些程序会显示黑屏）。\n你的原档案会被备份，随时点「还原默认」恢复。\n\n确认换到这个账号？`,
      { title: '目标账号是空的', okText: '仍然换号' })) return false;
  }
  let r = await run('swap_set', { app, name });
  if (r.noop) return true;
  // 要不要"结束进程再换号"是后端的事实：它知道自己被哪些进程挡住、结束它们能不能解开。
  // 以前前端拿中文文案做正则匹配，文案一换词按钮就再也不出现，提示却还写着"点强制换号"。
  if (!r.ok && r.need_force && await askConfirm(`${r.msg}\n\n确认由驾驶舱结束这些进程并换号？`,
    { danger: true, okText: '结束进程并换号' })) {
    r = await run('swap_set', { app, name, force: true });
  }
  return !!r.ok;
}
// 不能多开的产品：把"换到这个账号"和"打开它"合成一个动作
async function swapSetAndLaunch(app, name) {
  const a = S.data.apps.find((x) => x.id === app);
  if (a && a.shared_profile) return openClient(app, name);
  if (!await swapSet(app, name)) return false;
  await run('launch_app', { app, name });
  return true;
}
// 共用一份档案的产品（Qoder 系 / WorkBuddy / ZCode / TRAE 这类单窗口产品）：
// 切换账号＝在客户端里退出登录再登录。驾驶舱只把程序打开——不 swap_set、不结束任何进程，
// 因为她只是换个号，没有任何理由因此被要求关掉正在用的程序。
async function openClient(app, name) {
  const i = S.data.instances.find((x) => x.app === app && x.name === name);
  const a = S.data.apps.find((x) => x.id === app);
  const r = await invoke('action', { payload: { action: 'launch_app', app, name } })
    .catch((e) => ({ ok: false, msg: String(e) }));
  if (!r || !r.ok) { toast((r && r.msg) || '打开失败', false); return false; }
  toast(i && i.logged_in
    ? `已打开 ${a ? a.name : ''}`
    : `已打开 ${a ? a.name : ''}：在里面退出登录、再登「${name}」就行。\n`
      + '不需要关掉程序，驾驶舱也没有动任何文件；登进来之后这张卡会自己认出来。', true);
  load();
  return true;
}
// 一键换号：把目标号存过的登录态写回档案位置，再自动重开客户端。
// 这里"重启"是真的需要——客户端只在启动时读那份文件；和以前那种"文件根本没改却要求关程序"不是一回事。
async function authSwitch(app, name) {
  const a = S.data.apps.find((x) => x.id === app) || {};
  const cur = (S.data.instances || []).find((x) => x.app === app && x.logged_in);
  if (!await askConfirm(
    `切到「${name}」？\n\n把「${name}」存过的登录态写回 ${a.name || '该程序'} 的档案位置，然后自动重开它。\n`
    + (cur
      ? `当前登着的「${cur.name}」会在写之前先收好，随时切得回来。\n`
      : '还没读出当前登的是谁时后端不会动手（否则切过去就回不来了）。\n')
    + `会重启一次 ${a.name || '客户端'}：窗口里没保存的东西会丢。`,
    { title: '一键换号', okText: '切过去' })) return false;
  const r = await run('auth_switch', { app, name });
  return !!r.ok;
}
// 把早期 adopt 搬进数据根的档案搬回程序自己的位置（同盘改名，不复制）
async function rehomeAccount(app, name) {
  const i = S.data.instances.find((x) => x.app === app && x.name === name);
  if (!i || !i.rehome_to) return;
  if (!await askConfirm(
    `把「${name}」的档案搬回 ${i.rehome_to}？\n\n同盘改名，不复制内容。`
    + '搬完之后这个账号的数据就不再离开程序的默认位置，驾驶舱只保留账号名和额度记录。\n'
    + '需要先关闭该程序窗口。',
    { title: '把数据搬回原位', okText: '归位' })) return;
  let r = await run('rehome', { app, name });
  if (!r.ok && /还在跑|后台进程|个窗口/.test(r.msg)
    && await askConfirm(`${r.msg}`, { danger: true, okText: '结束进程并归位' })) {
    r = await run('rehome', { app, name, force: true });
  }
  return r.ok;
}
async function cleanCache(app, name) {
  if (await askConfirm(`清理「${name}」的缓存/日志/扩展缓存？登录态与设置保留。需先关闭该实例。`,
    { okText: '清理' })) {
    run('clean_cache', { app, name });
  }
}
async function del(app, name) {
  const i = S.data.instances.find((x) => x.app === app && x.name === name);
  // 接管进来的账号里那份数据可能是从默认档案搬过来的**唯一一份**，删掉就真没了 —— 必须说清
  const orphan = i && i.rehome_to
    ? `\n\n注意：这个账号的数据是当初从 ${i.seeded_from || '你本机的原有档案'} 搬进驾驶舱数据根的，别处没有第二份。\n想让它回到原来的位置不必删，用「账号设置 → 目录与数据 → 归位」。`
    : (i && i.origin === 'adopt' && !i.in_place
      ? '\n\n注意：这个账号的数据是接管进来的唯一一份，别处没有第二份。' : '');
  if (!await askConfirm(`删除账号「${name}」及其数据目录（含登录状态）？${orphan}\n\n删除前会自动打一个 zip 备份（可在「备份」里恢复）。`,
    { danger: true, okText: '删除账号' })) return false;
  const r = await run('delete', { app, name, backup: true });
  if (!r.ok && /备份/.test(r.msg)
    && await askConfirm(`${r.msg}`, { danger: true, okText: '不备份直接删' })) {
    const r2 = await run('delete', { app, name, backup: false });
    return r2.ok;
  }
  return r.ok;
}
async function killAll(pids, app) {
  if (await askConfirm(`将结束 ${app} 的全部 ${pids.length}+ 个进程——包括你正在使用的窗口！\n只关单个账号请回到卡片用「■ 结束」。\n确认继续？`,
    { danger: true, okText: '全部结束' })) {
    run('kill_pids', { pids });
  }
}
function proxyToggle() {
  if (S.data.proxy.running) run('proxy_stop');
  else run('proxy_start', { upstream: $('p-up').value, strategy: $('p-st').value });
}

function bind() {
  $('btn-settings').addEventListener('click', () => {
    S.view = S.view === 'settings' ? 'agents' : 'settings';
    render();
  });
  $('c-alerts').addEventListener('click', showAlerts);
  bindCtx();
  $('btn-new').addEventListener('click', openWizard);
  $('btn-scan').addEventListener('click', () => run('rescan'));
  $('btn-inv').addEventListener('click', async () => {
    const b = $('btn-inv');
    b.disabled = true;
    const old = b.textContent;
    b.textContent = '盘点中…';
    let r;
    try {
      r = await invoke('scan_inventory');
    } catch (e) {
      toast(`盘点失败：${e}`, false);
      b.disabled = false; b.textContent = old;
      return;
    }
    b.disabled = false; b.textContent = old;
    S.inv = r;
    toast(r.first_run
      ? `本机盘点完成：认出 ${r.detected} 家（第一次跑，只存基线，不报增减）`
      : `本机盘点完成：${r.detected} 家 · 新装 ${r.new.length} · 卸载 ${r.gone.length}`
        + (r.stale_paths.length ? ` · 内置路径失效 ${r.stale_paths.length}` : ''));
    setSection = 'advanced';
    S.view = 'settings';
    render();
  });
  $('btn-hide').addEventListener('click', async () => {
    const a = S.data.apps.find((x) => x.id === S.cur);
    if (!a) return;
    if (await askConfirm(`把「${a.name}」从列表隐藏？\n不会卸载程序，也不动它的账号目录；可在 设置 → 高级 → 已隐藏的程序 里恢复。`,
      { okText: '隐藏' })) {
      run('hide_app', { app: a.id });
      S.cur = null;
    }
  });
  $('btn-forget').addEventListener('click', async () => {
    const a = S.data.apps.find((x) => x.id === S.cur);
    if (!a) return;
    if (await askConfirm(`移除「${a.name}」的卸载记录？账号目录原样保留。`, { danger: true, okText: '清理' })) {
      run('forget_app', { app: a.id });
      S.cur = null;
    }
  });
  $('btn-add-app').addEventListener('click', openAddApp);
  $('im-cancel').addEventListener('click', closeDetail);
  $('im-x').addEventListener('click', closeDetail);
  $('im-save').addEventListener('click', saveDetail);
  $('imodal').addEventListener('click', (e) => { if (e.target.id === 'imodal') closeDetail(); });
  $('am-cancel').addEventListener('click', () => { $('amodal').hidden = true; });
  $('amodal').addEventListener('click', (e) => { if (e.target.id === 'amodal') $('amodal').hidden = true; });
  $('am-add').addEventListener('click', doAddApp);
  ['am-exe', 'am-name'].forEach((id) => $(id).addEventListener('keydown',
    (e) => { if (e.key === 'Enter') doAddApp(); }));
  $('am-scan').addEventListener('click', () => { $('amodal').hidden = true; run('rescan'); });
  $('ask-ok').addEventListener('click', askSubmit);
  $('ask-cancel').addEventListener('click', () => askDone(null));
  $('ask').addEventListener('click', (e) => { if (e.target.id === 'ask') askDone(null); });
  $('btn-reload').addEventListener('click', load);
  $('p-btn').addEventListener('click', proxyToggle);
  $('gp-cancel').addEventListener('click', closePoolModal);
  $('gp-save').addEventListener('click', savePoolModal);
  $('gpmodal').addEventListener('click', (e) => { if (e.target.id === 'gpmodal') closePoolModal(); });
  $('gp-name').addEventListener('keydown', (e) => { if (e.key === 'Enter') savePoolModal(); });
  $('gd-cancel').addEventListener('click', closeDistModal);
  $('gd-save').addEventListener('click', saveDistModal);
  $('gdmodal').addEventListener('click', (e) => { if (e.target.id === 'gdmodal') closeDistModal(); });
  $('gd-name').addEventListener('keydown', (e) => { if (e.key === 'Enter') saveDistModal(); });
  $('qm-close').addEventListener('click', closeQuotaDetail);
  $('qm-save').addEventListener('click', savePendingQuota);
  $('qmodal').addEventListener('click', (e) => { if (e.target.id === 'qmodal') closeQuotaDetail(); });
  $('btn-key-new').addEventListener('click', () => openSiteModal(null));
  $('btn-keys-expiring').addEventListener('click', () => {
    S.keysSoonOnly = !S.keysSoonOnly;
    $('btn-keys-expiring').classList.toggle('primary', S.keysSoonOnly);
    renderKeys(S.data);
  });
  $('btn-keys-dedup').addEventListener('click', () => openKeyBatch('dedup'));
  $('btn-keys-batch').addEventListener('click', () => openKeyBatch('all'));
  $('btn-checkin-refresh').addEventListener('click', () => loadCheckin(true));
  $('c-checkin').addEventListener('click', () => { S.view = 'checkin'; render(); });
  $('btn-checkin-all').addEventListener('click', async () => {
    const items = ((S.checkin || {}).items || []).filter((x) => x.claimable === true);
    if (!items.length) { toast('今天没有可领的', false); return; }
    const names = items.map((x) => CI_NAMES[x.product] || x.product).join('、');
    const yes = await askConfirm(
      `要给这几家领今天的签到积分：\n${names}\n\n领完当日名额就用掉了，撤回不了。`,
      { title: '确认领取' });
    if (!yes) return;
    const bad = [];
    for (const x of items) {
      const r = await invoke('action', { payload: { action: 'checkin_claim', app: '', store: x.product } });
      const res = (r && r.result) || {};
      if (!res.claimed) bad.push(`${CI_NAMES[x.product] || x.product}：${res.error || res.message || '没领成'}`);
    }
    // "已提交"这种话不算结果——领没领要逐家报，不然又是一堆假成功
    toast(bad.length ? `${items.length - bad.length} 家领到了，${bad.length} 家没领成：\n${bad.join('\n')}`
                     : `${items.length} 家都领到了`, !bad.length);
    loadCheckin(true);
  });
  $('bd-cancel').addEventListener('click', () => { $('bdmodal').hidden = true; });
  $('bdmodal').addEventListener('click', (e) => { if (e.target.id === 'bdmodal') $('bdmodal').hidden = true; });
  $('bd-go').addEventListener('click', bdDelete);
  $('sm-cancel').addEventListener('click', closeSiteModal);
  $('sm-save').addEventListener('click', saveSiteModal);
  $('smodal').addEventListener('click', (e) => { if (e.target.id === 'smodal') closeSiteModal(); });
  ['s-name', 's-home'].forEach((id) => $(id).addEventListener('keydown',
    (e) => { if (e.key === 'Enter') saveSiteModal(); }));
  $('km-cancel').addEventListener('click', closeKeyModal);
  // 别直接把函数当 handler：click 事件会被当成 saveKeyModal 的 again 参数
  $('km-save').addEventListener('click', () => saveKeyModal());
  $('km-next').addEventListener('click', () => saveKeyModal(true));
  $('kmodal').addEventListener('click', (e) => { if (e.target.id === 'kmodal') closeKeyModal(); });
  $('km-test').addEventListener('click', async () => {
    const item = collectKeyItem();
    if (!item.key && !keyEditing.key.id) { toast('先填 Key 原文', false); return; }
    // 测试前先落盘（新建时用刚填的 key；编辑时后端会保留原 key）
    const s = await invoke('action', { payload: { action: 'key_upsert_key', site: keyEditing.site.id, item } });
    if (!s.ok) { toast(s.msg, false); return; }
    keyEditing.key.id = item.id;
    const r = await invoke('action', { payload: { action: 'key_test', site: keyEditing.site.id, id: item.id } });
    toast(r.msg, r.ok);
    load();
  });
  $('k-intake-go').addEventListener('click', () => intakeRegister());
  $('k-intake-img').addEventListener('click', () => $('k-intake-file').click());
  $('k-intake-file').addEventListener('change', (e) => {
    const f = e.target.files && e.target.files[0];
    if (!f) return;
    fileToDataUrl(f).then((u) => intakeRegister(u)).catch((err) => toast(err.message, false));
    e.target.value = '';
  });
  $('inj-cancel').addEventListener('click', () => { $('injmodal').hidden = true; });
  $('inj-go').addEventListener('click', injectApply);
  $('injmodal').addEventListener('click', (e) => { if (e.target.id === 'injmodal') $('injmodal').hidden = true; });
  $('wz-next').addEventListener('click', wizardNext);
  $('wz-back').addEventListener('click', () => {
    S.wizard.step = Math.max(0, S.wizard.step - 1); renderWizard();
  });
  $('wz-cancel').addEventListener('click', closeWizard);
  $('tour-next').addEventListener('click', () => {
    if (S.tour.i >= TOUR.length - 1) endTour();
    else { S.tour.i += 1; renderTour(); }
  });
  $('tour-skip').addEventListener('click', endTour);
  document.querySelectorAll('.back-agents').forEach((b) => {
    b.addEventListener('click', () => { S.view = 'agents'; render(); });
  });
  document.querySelectorAll('#viewlist [data-view]').forEach((b) => {
    b.addEventListener('click', () => {
      S.view = b.dataset.view;
      render();
      if (S.view === 'collab') ensureCollab(false);
    });
  });
  $('btn-home-reload').addEventListener('click', () => load());
  $('btn-collab-retry').addEventListener('click', () => ensureCollab(true));
  $('btn-collab-retry2').addEventListener('click', () => ensureCollab(true));
  // 窗口控制
  $('w-min').addEventListener('click', () => win.minimize());
  $('w-max').addEventListener('click', () => win.toggleMaximize());
  $('w-close').addEventListener('click', hideToTray);
  win.onResized(() => {
    win.isMaximized().then((m) => {
      document.querySelector('.winbtns').classList.toggle('maximized', m);
      $('w-max').title = m ? '向下还原' : '最大化';
    }).catch(() => {});
  });
  bindNavResize();
  bindSideResize();
  bindAiPaste();
  $('btn-nav-toggle').addEventListener('click', () => {
    setSetting('appearance', 'nav_collapsed', !document.documentElement.classList.contains('rail'));
  });
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') {
      if (!$('tour').hidden) endTour();
      else if (!$('qmodal').hidden) closeQuotaDetail();
      else if (!$('wizard').hidden) closeWizard();
      else if (!$('imodal').hidden) closeDetail();
      else if (!$('amodal').hidden) $('amodal').hidden = true;
      else if (!$('gpmodal').hidden) closePoolModal();
      else if (!$('gdmodal').hidden) closeDistModal();
      else if (!$('kmodal').hidden) closeKeyModal();
      else if (!$('injmodal').hidden) $('injmodal').hidden = true;
      else if (S.view !== 'agents') { S.view = 'agents'; render(); }
    }
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'r') { e.preventDefault(); load(); }
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'b') { e.preventDefault(); $('btn-nav-toggle').click(); }
  });
}

function hideToTray() {
  const exit = S.data?.settings?.general?.close_action === 'exit';
  if (exit) { win.close(); return; }
  win.hide();
  toast('已收进系统托盘，后台继续监控额度；托盘菜单可退出');
}

/* 轮询间隔跟随设置 */
let pollTimer = null;
function schedulePoll() {
  if (pollTimer) clearInterval(pollTimer);
  const sec = Math.max(2, Math.min(60, Number(S.data?.settings?.general?.refresh_sec) || 4));
  pollTimer = setInterval(() => { if (!document.hidden) load(); }, sec * 1000);
}

async function init() {
  if (!window.__TAURI__) await import('./mock.js');
  invoke = window.__TAURI__.core.invoke;
  win = window.__TAURI__.window.getCurrentWindow();
  bind();
  await load();
  // 主题搬进设置后，把旧版存在 localStorage 里的偏好迁移过来，别让默认值盖掉用户的选择
  const legacy = localStorage.getItem('cockpit-theme');
  if (legacy) {
    localStorage.removeItem('cockpit-theme');
    await setSetting('appearance', 'theme', legacy);
    toast(`已沿用你之前的主题：${{ light: '浅色', dark: '深色', system: '跟随系统' }[legacy] || legacy}`);
  }
  await win.onCloseRequested((event) => {
    if (S.data?.settings?.general?.close_action === 'exit') return; // 交给系统默认关闭
    event.preventDefault();
    hideToTray();
  });
  if (!localStorage.getItem('cockpit-toured')) setTimeout(startTour, 600);
  schedulePoll();
}

init();
