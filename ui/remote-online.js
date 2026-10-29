/* 公网远控入口：桌面明确开启后才建立受邮箱验证码保护的临时中继。 */
(() => {
  const originalRemoteSection = remoteSection;
  const originalRenderRemotePage = renderRemotePage;
  let verificationEmail = '';
  let actionBusy = false;
  let restoreEmailFocus = false;

  renderRemotePage = function () {
    restoreEmailFocus = document.activeElement?.id === 'remote-online-email';
    originalRenderRemotePage();
  };

  async function runRemoteAction(action, payload = {}) {
    if (actionBusy) return;
    actionBusy = true;
    try {
      const result = await invoke('action', { payload: { action, ...payload } });
      toast(result?.msg || (result?.ok ? '操作成功' : '操作失败'), !!result?.ok);
    } catch (error) {
      toast(error?.message || '操作失败，请重试', false);
    } finally {
      actionBusy = false;
      await loadRemoteView(true);
    }
  }

  remoteSection = function (body, panel, row, actBtn) {
    originalRemoteSection(body, panel, row, actBtn);
    const view = S.remoteView;
    if (!view) return;

    const tunnel = view.tunnel || { state: 'stopped', message: '公网远控未开启' };
    const active = ['starting', 'ready', 'reconnecting'].includes(tunnel.state);
    const controls = [];
    if (!view.enabled) {
      controls.push(el('div', { class: 'invline' }, '请先开启上方的远程控制，再建立公网连接。'));
    } else if (!active) {
      if (tunnel.state === 'failed') controls.push(el('div', { class: 'invline warn' }, tunnel.message || '公网连接失败，请重试。'));
      const input = el('input', {
        id: 'remote-online-email', type: 'email', value: verificationEmail,
        placeholder: '接收验证码的邮箱', autocomplete: 'email',
        style: 'width: min(100%, 320px); box-sizing: border-box;',
        oninput: (event) => { verificationEmail = event.target.value; },
      });
      controls.push(row('验证邮箱', '手机访问时需要输入该邮箱收到的一次性验证码', input));
      controls.push(row('公网连接', '通过 Cloudflare 建立临时连接', actBtn('开启公网远控', () => {
        verificationEmail = input.value.trim();
        if (!verificationEmail) { toast('请输入接收验证码的邮箱', false); input.focus(); return; }
        return runRemoteAction('remote_tunnel_start', { email: verificationEmail });
      })));
      if (restoreEmailFocus) queueMicrotask(() => {
        if (input.isConnected) {
          input.focus();
        }
      });
    } else {
      controls.push(row('连接', tunnel.message || '正在连接', actBtn('停止公网远控', () => runRemoteAction('remote_tunnel_stop'))));
      if (tunnel.state === 'ready') {
        const pair = view.pair;
        const publicPair = view.public_pair;
        if (publicPair && pair?.code && pair.expires_in > 0) {
          controls.push(el('div', { class: 'invline' }, '用手机扫码或复制链接打开，输入邮箱验证码后完成配对。'));
          controls.push(el('img', {
            src: 'data:image/svg+xml;utf8,' + encodeURIComponent(publicPair.qr_svg || ''),
            alt: '公网远控配对二维码', width: 190, height: 190,
            style: 'background:#fff;border-radius:12px;padding:8px;display:block',
          }));
          controls.push(el('div', { class: 'kv' }, publicPair.url));
          controls.push(row('连接链接', '链接包含一次性配对码，请仅分享给本人设备', actBtn('复制链接', () => copyText(publicPair.url))));
          controls.push(el('div', { class: 'kv' }, '配对码：', el('b', {}, pair.code), '。如邮箱验证后没有自动配对，请在手机上输入此码。'));
        } else {
          controls.push(el('div', { class: 'invline' }, '当前没有有效配对码，请生成新码后扫码。'));
        }
        controls.push(row('配对码', '新码生成后，未使用的旧码会失效', actBtn('生成配对码', () => runRemoteAction('remote_code'))));
      }
    }
    body.appendChild(panel('公网远程控制',
      el('div', { class: 'invline' }, '手机可通过移动网络或其他 Wi‑Fi 访问；电脑和驾驶舱需要保持运行并联网。'),
      el('div', { class: 'invline' }, '连接由 Cloudflare 临时中继转发，地址在重新开启后会变化，服务可用性没有保证。'),
      ...controls));
  };
})();
