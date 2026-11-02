/* 法律声明与风险告知：应用内唯一来源。
   设置「关于」页渲染摘要，反代页顶部用 proxyRiskNote() 挂常驻提示，
   首次启用反代时由 ensureProxyConsent() 拦一次（勾选后才放行）。
   确认结果存在 settings.legal.proxyAccepted（config.json），只弹一次。

   文案规矩：一句只服务当前这一步；长内容走「完整条款」链接，弹窗说明不超过两句。 */

const LEGAL_REPO = 'https://github.com/shen1950/agent-cockpit';
const LEGAL_DOC_PATH = '/blob/HEAD/docs/LEGAL.md';
const PROXY_DOC_PATH = '/blob/HEAD/docs/PROXY.md';

/* 一句话结论：任何地方要引用免责，都从这里取，不要各自写一份。 */
const LEGAL_ONE_LINE = '本软件按「原样」提供，使用后果由使用者自行承担，作者不承担任何责任。';

/* 关于页的三条要点：只列使用者最需要知道的，完整条款走链接。 */
const LEGAL_POINTS = [
  ['合规自负', '只操作你自己的账号与凭据；多开、换号、反代是否合规，由你按各自条款判断。'],
  ['反代风险最高', '反代可能违反上游服务条款，导致限流、封号或额度清零，作者不承担责任。'],
  ['数据仅供参考', '额度与识别结果可能延迟或有误，以各产品官方页面为准。'],
];

/* 反代页常驻提示。两句讲完风险与责任归属，细节交给 PROXY.md。 */
const PROXY_RISK_LINE = '反代只在本机转发你自己的账号凭据，不改变上游的授权与计费规则。'
  + '多数产品条款禁止账号共享或自动化访问，被判定违规时的限流、封号、额度清零等后果由你自行承担，作者不负责。';

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
      legalLink('完整条款', LEGAL_DOC_PATH)));
}

/* 首次启用反代前的一次性强确认：必须勾选才能继续。返回 Promise<boolean>。
   只拦「第一次」——之后由 settings.legal.proxyAccepted 放行，不再打扰。 */
function ensureProxyConsent() {
  if (legalProxyAccepted()) return Promise.resolve(true);
  return new Promise((resolve) => {
    const ack = el('input', { type: 'checkbox', id: 'legal-ack' });
    const ok = el('button', { class: 'btn primary', disabled: true }, '我已了解，启用反代');
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
      el('h3', {}, '启用反代前，请先确认'),
      el('p', { class: 'askbody' }, PROXY_RISK_LINE),
      el('p', { class: 'askbody legal-fine' }, '分发 Key 等同于你的额度钥匙，请勿外传；也不要把 8788 端口暴露到公网。'),
      el('label', { class: 'legalack' }, ack, el('span', {}, '我已阅读上述说明，并自行承担由此产生的全部后果')),
      el('div', { class: 'legal-fine' }, legalLink('完整条款', LEGAL_DOC_PATH), ' · ', legalLink('反代风险', PROXY_DOC_PATH)),
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
