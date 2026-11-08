/* 法律声明与风险告知：应用内唯一来源。
   设置「关于」页渲染摘要，反代页顶部用 proxyRiskNote() 挂常驻提示，
   首次启用反代时由 ensureProxyConsent() 拦一次（勾选后才放行）。
   确认结果存在 settings.legal.proxyAccepted（config.json），只弹一次。

   文案规矩：一句只服务当前这一步；长内容走「完整条款」链接，弹窗说明不超过两句。 */

const LEGAL_REPO = 'https://github.com/shen1950/agent-cockpit';
const LEGAL_DOC_PATH = '/blob/HEAD/docs/LEGAL.md';
const PROXY_DOC_PATH = '/blob/HEAD/docs/PROXY.md';

/* 一句话结论：任何地方要引用免责，都从这里取，不要各自写一份。 */
const LEGAL_ONE_LINE = '本软件按「原样」提供；担保与责任限制以 MIT 许可证和适用法律为准。';

/* 关于页的三条要点：只列使用者最需要知道的，完整条款走链接。 */
const LEGAL_POINTS = [
  ['合规自负', '只操作你自己的账号与凭据；多开、换号、反代是否合规，由你按各自条款判断。'],
  ['反代风险', '反代可能触及上游服务条款，导致限流、封号或额度损失；使用前请核对相应条款。'],
  ['数据仅供参考', '额度与识别结果可能延迟或有误，以各产品官方页面为准。'],
];

/* 反代页常驻提示。两句讲完风险与责任归属，细节交给 PROXY.md。 */
const PROXY_RISK_LINE = '网关使用你配置的账号转发请求，不改变上游的授权与计费规则。'
  + '请核对所用产品对账号共享和自动化访问的限制，并妥善保管接入密钥。责任限制见完整条款。';

/* 首次启用反代时必须逐条同意的协议。每条一句，能读懂、能追责；
   别改写成「本人已知晓风险」这种什么都没说的空话。 */
const PROXY_TERMS = [
  '本软件按「原样」提供；担保与责任限制在适用法律允许的范围内适用，不能免除依法不可免除的责任。',
  '反代只在本机转发你自己的请求，用的是你自己已登录的账号凭据与额度，不改变上游的授权与计费规则。',
  '多开、换号与反代在部分产品上可能违反其用户协议，是否合规由你自行判断并负责。',
  '若被上游判定为违规使用，由此产生的限流、封号、订阅失效、额度或积分清零等后果由你自行承担。',
  '分发 Key 等同于你的额度钥匙，请勿外传；也不要把 8788 端口暴露到公网或借给他人使用。',
];

function openLegalDoc(path = LEGAL_DOC_PATH) {
  if (!window.__TAURI_INTERNALS__) {
    window.open(LEGAL_REPO + path, '_blank', 'noopener');
    return;
  }
  invoke('action', { payload: { action: 'open_project_link', path } })
    .then((r) => { if (r && r.ok === false) toast(r.msg, false); })
    .catch(() => toast('打开文档失败', false));
}

function legalLink(label, path, cls = '') {
  const a = el('a', { class: `legal-link ${cls}`.trim(), href: LEGAL_REPO + path, target: '_blank', rel: 'noopener noreferrer' }, label);
  a.addEventListener('click', (ev) => {
    if (!window.__TAURI_INTERNALS__) return;   // 浏览器预览：让它照常跳转
    ev.preventDefault();
    openLegalDoc(path);
  });
  return a;
}

function legalProxyAccepted() {
  return S.data?.settings?.legal?.proxyAccepted === true;
}

/* 写盘失败不挡使用——只是下次还会再问一遍。 */
async function rememberProxyConsent() {
  try {
    const r = await invoke('set_settings', { patch: { legal: { proxyAccepted: true } } });
    if (r && S.data) S.data.settings = r;
  } catch (e) { /* 下次再问一次即可 */ }
}

/* 反代页顶部的常驻风险条：常显，不折叠，不靠悬停才看得见。 */
function proxyRiskNote() {
  return el('div', { class: 'legalnote' },
    el('div', { class: 'legalnote-main' },
      el('b', {}, '用前须知'),
      el('p', {}, PROXY_RISK_LINE)),
    el('div', { class: 'legalnote-acts' },
      legalLink('风险与配置', PROXY_DOC_PATH),
      legalLink('完整条款', LEGAL_DOC_PATH),
      legalLink('开源来源与许可', '/blob/HEAD/THIRD_PARTY_NOTICES.md')));
}

/* 首次启用反代前的一次性强确认：必须勾选才能继续。返回 Promise<boolean>。
   默认只拦「第一次」——之后由 settings.legal.proxyAccepted 放行，不再打扰。
   force=true 无视已同意状态再弹一次（设置页的「重新查看协议」用）。 */
function ensureProxyConsent(force = false) {
  if (!force && legalProxyAccepted()) return Promise.resolve(true);
  return new Promise((resolve) => {
    const ack = el('input', { type: 'checkbox', id: 'legal-ack' });
    const ok = el('button', { class: 'btn primary', disabled: true }, '同意并启用反代');
    const cancel = el('button', { class: 'btn' }, '取消');
    const onKey = (ev) => { if (ev.key === 'Escape') { ev.stopPropagation(); close(false); } };
    function close(value) {
      document.removeEventListener('keydown', onKey, true);
      back.remove();
      resolve(value);
    }
    ok.addEventListener('click', () => close(true));
    cancel.addEventListener('click', () => close(false));
    ack.addEventListener('change', () => { ok.disabled = !ack.checked; });

    const dialog = el('div', { class: 'dialog legaldialog' },
      el('h3', {}, '启用本地反代前，请先同意以下条款'),
      el('ol', { class: 'legalterms' }, ...PROXY_TERMS.map((text) => el('li', {}, text))),
      el('label', { class: 'legalack' }, ack, el('span', {}, '我已阅读并同意上述全部条款，自愿承担相应风险')),
      el('div', { class: 'legal-fine' }, legalLink('完整免责条款', LEGAL_DOC_PATH), ' · ', legalLink('反代风险说明', PROXY_DOC_PATH)),
      el('footer', { class: 'dialogfoot' }, cancel, ok));
    dialog.setAttribute('role', 'dialog');
    dialog.setAttribute('aria-modal', 'true');

    const back = el('div', { class: 'modal' }, dialog);
    back.style.zIndex = '10000';
    document.body.appendChild(back);
    document.addEventListener('keydown', onKey, true);
    ack.focus();
  }).then(async (yes) => {
    if (yes) await rememberProxyConsent();
    return yes;
  });
}
