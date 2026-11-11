/* One resolved icon for all application surfaces. Custom images stay on this device. */
let brandSource = '', brandRevision = 0;
let brandNativeQueue = Promise.resolve();
function brandVariant(a = {}) {
  return ['light', 'dark'].includes(a.icon_tone) ? a.icon_tone : document.documentElement.dataset.theme || 'light';
}
function brandUrl(a = {}) {
  const tone = brandVariant(a);
  const custom = a[`icon_custom_${tone}`];
  return a.icon_style === 'custom' && typeof custom === 'string' && custom.startsWith('data:image/png;base64,')
    ? custom : `brand-${tone}.png`;
}
async function brandCanvas(src, size = 256) {
  const image = new Image(); image.src = src; await image.decode();
  if (!image.width || !image.height || image.width * image.height > 40000000) throw new Error('图片尺寸过大或无法读取');
  const canvas = document.createElement('canvas'); canvas.width = canvas.height = size;
  const ctx = canvas.getContext('2d');
  const scale = Math.min(size / image.width, size / image.height);
  const width = image.width * scale, height = image.height * scale;
  ctx.drawImage(image, (size-width)/2, (size-height)/2, width, height);
  return canvas;
}
function applyBrand(a = {}) {
  const src = brandUrl(a);
  document.querySelectorAll('[data-brand-icon]').forEach(node => { if (node.getAttribute('src') !== src) node.src = src; });
  let favicon = document.querySelector('link[rel="icon"]');
  if (!favicon) { favicon = document.createElement('link'); favicon.rel = 'icon'; document.head.append(favicon); }
  favicon.href = src;
  if (src === brandSource) return;
  brandSource = src; const revision = ++brandRevision;
  if (!window.__TAURI_INTERNALS__) return;
  brandNativeQueue = brandNativeQueue.catch(() => {}).then(async () => {
    if (revision !== brandRevision) return;
    const canvas = await brandCanvas(src);
    if (revision !== brandRevision) return;
    await invoke('set_brand_icon', { rgba: Array.from(canvas.getContext('2d').getImageData(0,0,256,256).data) });
  }).catch(error => { if (revision === brandRevision) { brandSource = ''; toast(`系统图标更新失败：${error}`, false); } });
}
async function importBrand(tone, file) {
  if (!file) return;
  if (!['image/png', 'image/jpeg', 'image/webp'].includes(file.type) || file.size > 8*1024*1024) throw new Error('请选择 8 MB 以内的 PNG、JPEG 或 WebP 图片');
  const url = URL.createObjectURL(file);
  try {
    const canvas = await brandCanvas(url, 256);
    await setSetting('appearance', `icon_custom_${tone}`, canvas.toDataURL('image/png'));
    await setSetting('appearance', 'icon_style', 'custom');
  } finally { URL.revokeObjectURL(url); }
}
function brandSettings(body, panel, a) {
  const select = el('select', { aria: { label: '图标外观' } });
  for (const [value,label] of [['auto','跟随主题'],['light','浅色'],['dark','深色']]) select.append(el('option', { value }, label));
  select.value = a.icon_tone || 'auto';
  select.addEventListener('change', () => setSetting('appearance', 'icon_tone', select.value));
  const cards = el('div', { class: 'brand-choices' });
  for (const [style, label] of [['copper','Copper 导航'], ['custom','自定义图标']]) {
    const card = btn('', 'brand-choice', () => setSetting('appearance', 'icon_style', style));
    card.setAttribute('aria-pressed', String((a.icon_style || 'copper') === style));
    card.setAttribute('aria-label', label);
    const preview = el('div', { class: 'brand-pair' });
    for (const tone of ['light','dark']) preview.append(el('div', { class: `brand-sample ${tone}` },
      el('img', { src: style === 'custom' && a[`icon_custom_${tone}`] || `brand-${tone}.png`, alt: `${label} · ${tone === 'light' ? '浅色' : '深色'}` }),
      el('span', {}, tone === 'light' ? '浅色' : '深色')));
    card.append(preview, el('span', { class: 'brand-choice-label' }, label)); cards.append(card);
  }
  const uploads = el('div', { class: 'settings-actions' });
  for (const tone of ['light','dark']) {
    const input = el('input', { type: 'file', accept: 'image/png,image/jpeg,image/webp', hidden: true, aria: { label: `上传${tone === 'light' ? '浅色' : '深色'}图标` } });
    input.addEventListener('change', () => importBrand(tone,input.files[0]).catch(e => toast(e.message,false)));
    uploads.append(input, btn(`更换${tone === 'light' ? '浅色' : '深色'}图片`, '', () => input.click()));
  }
  uploads.append(btn('恢复 Copper 默认图标', '', async () => {
    await setSetting('appearance','icon_style','copper'); await setSetting('appearance','icon_tone','auto');
  }));
  body.append(panel('应用图标', row('图标外观', '默认跟随应用主题，也可以固定浅色或深色。', select), cards, uploads,
    el('p', { class: 'settings-copy' }, '自定义图片仅保存在本机，未上传的一侧沿用 Copper。窗口、托盘、应用内图标即时更新；桌面、开始菜单和任务栏固定项会在同一时刻被改写成当前这张图并刷新图标缓存（每次启动也会自查一遍，发现指歪的条目自动拉回）。任务栏上如果还挂着旧图，取消固定再固定一次即可，之后不会再变。')));
}
