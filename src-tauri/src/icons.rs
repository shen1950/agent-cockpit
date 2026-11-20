// 提取 exe 的真实图标，缓存成 PNG，前端用 data URI 直接显示
use crate::registry::base_dir;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn cache() -> &'static Mutex<HashMap<String, String>> {
    static C: std::sync::OnceLock<Mutex<HashMap<String, String>>> = std::sync::OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn icon_dir() -> PathBuf {
    base_dir().join("icons")
}

fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 中文名转下划线后会互相撞车，所以再拼一段哈希后缀
fn safe_key(key: &str) -> String {
    let ascii: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect();
    let head: String = ascii.chars().take(24).collect();
    format!("{}-{:08x}", head, (fnv(key) & 0xffff_ffff) as u32)
}

fn file_for(key: &str) -> PathBuf {
    icon_dir().join(format!("{}.png", safe_key(key)))
}

fn data_uri(png: &Path) -> Option<String> {
    let bytes = fs::read(png).ok()?;
    if bytes.is_empty() {
        return None;
    }
    Some(format!("data:image/png;base64,{}", base64(&bytes)))
}

/// 极简 base64（避免为一个小函数再引依赖）
fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        s.push(T[(n >> 18 & 63) as usize] as char);
        s.push(T[(n >> 12 & 63) as usize] as char);
        s.push(if chunk.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        s.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    s
}

/// 脚本文件自己没有图标资源，ExtractAssociatedIcon 对它们只能给出 Windows 那张
/// 「空白文档」默认图——存下来前端会当成真图标显示（Codex CLI 的白纸就是这么来的）。
/// 这类目标一律不抽，让界面回退到内置品牌 SVG 或名字缩写。
fn carries_own_icon(path: &str) -> bool {
    !matches!(
        Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("cmd") | Some("bat") | Some("ps1") | Some("vbs") | Some("js") | Some("mjs")
            | Some("ts") | Some("py") | Some("sh")
    )
}

/// MSIX 包的主程序图标常常是壳自带的占位图——ChatGPT.exe 用 ExtractAssociatedIcon
/// 抽出来是个绿方块，不是 OpenAI 那个花标。厂商自己的 logo 在包根 `assets/` 下，
/// 有就直接拷这张：比抽壳图标准，也不用起 PowerShell。
///
/// 候选按**尺寸从大到小**挑，不是按名字顺序：包里同时有 16/32/48/…/256 和
/// scale-125/150/200/400 若干档，早先写死的那个列表先命中的常常是 32 或 88 的小图，
/// 贴在 28px 的侧栏格子里就是一团糊。这里在包里所有 PNG 里挑**短边最大的那张**。
fn msix_logo(exe: &Path) -> Option<PathBuf> {
    let root = exe.parent()?.parent()?; // <包根>/app/ChatGPT.exe → <包根>
    let assets = root.join("assets");
    let mut best: Option<(u32, PathBuf)> = None;
    let rd = fs::read_dir(&assets).ok()?;
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()).map(|x| x.to_ascii_lowercase())
            != Some("png".into())
        {
            continue;
        }
        // 包自己的 logo 命名：Square44x44Logo.* / Square150x150Logo.* / Square310x310Logo.*
        // / icon.png / Wide310x150Logo.*。最后两类是横图和启动图，不是图标，跳过。
        let name = p.file_name()?.to_string_lossy().to_ascii_lowercase();
        if !name.starts_with("square") || name.contains("wide") {
            continue;
        }
        if let Some(edge) = png_edge(&p) {
            if best.as_ref().map(|(b, _)| edge > *b).unwrap_or(true) {
                best = Some((edge, p));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// 用户自己指定的图标：`icons/custom/<产品id>.png`。放进去就生效，
/// 优先于包内 logo 和 exe 抽取——有些产品抽出来/自带的都不是想要的那张。
fn custom_icon(app_id: &str) -> Option<PathBuf> {
    let p = icon_dir().join("custom").join(format!("{app_id}.png"));
    p.is_file().then_some(p)
}

/// 这个产品的图标是不是用户自己指定的（前端要拿这个让自定义图盖过内置品牌 SVG）。
pub fn is_custom(app_id: &str) -> bool {
    custom_icon(app_id).is_some()
}

/// 缓存图标的最短边。小于这个值就当废图——MSIX 的壳 exe（ChatGPT.exe 就是）
/// 只能抽出 32×32 的那一档，贴在侧栏 28px 的格子里再被高 DPI 拉一次，糊成一团灰。
const MIN_EDGE: u32 = 64;

/// 读 PNG 头里的宽高取小边。IHDR 在固定偏移 16/20（8 字节签名 + 4 长度 + 4 "IHDR"），
/// 都是大端。自己解析，别为这一个尺寸把 png 解码器拖进依赖。
fn png_edge(path: &Path) -> Option<u32> {
    use std::io::Read;
    let mut head = [0u8; 24];
    let mut f = fs::File::open(path).ok()?;
    f.read_exact(&mut head).ok()?;
    if &head[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes([head[16], head[17], head[18], head[19]]);
    let h = u32::from_be_bytes([head[20], head[21], head[22], head[23]]);
    Some(w.min(h))
}

/// 这张缓存图小得没法看吗（小图 → 抽的时候没抽到大的，要重抽）。
fn undersized(png: &Path) -> bool {
    png_edge(png).map(|e| e < MIN_EDGE).unwrap_or(false)
}

/// 把包里的 logo 拷进缓存目录。
///
/// **不能用 `fs::copy`**：WindowsApps 下的文件带 EFS 加密标记，Windows 会拿源文件的
/// 加密属性去套目标文件，于是报 6000「无法加密指定的文件」直接失败
/// （实测 `C:\Program Files\WindowsApps\OpenAI.Codex_*\assets\*.png` 全都这样）。
/// 老代码正是栽在这里：`copy` 失败被 `is_ok()` 吞掉，静默退回抽壳 exe，
/// 抽出来的是 32×32 的那一档——侧栏里那张糊图就是这么来的。
///
/// 读字节再自己写就没这问题：读只涉及解密，写的是全新文件、不带加密属性。
fn copy_logo(src: &Path, dst: &Path) -> std::io::Result<u64> {
    let bytes = fs::read(src)?;
    fs::write(dst, &bytes)?;
    Ok(bytes.len() as u64)
}

/// 一次性把所有缺失的图标抽出来（单个 PowerShell 进程，避免逐个启动）
pub fn refresh(items: &[(String, String)]) {
    let _ = fs::create_dir_all(icon_dir());
    let mut missing: Vec<(String, String)> = Vec::new();
    for (k, exe) in items {
        let dst = file_for(k);
        // ① 手工指定的图标最优先：只比修改时间，换一张新的不用先删缓存
        if let Some(ci) = custom_icon(k) {
            let newer = fs::metadata(&ci)
                .and_then(|m| m.modified())
                .ok()
                .zip(fs::metadata(&dst).and_then(|m| m.modified()).ok())
                .map(|(a, b)| a > b)
                .unwrap_or(true);
            if newer {
                let _ = fs::copy(&ci, &dst);
            }
            continue;
        }
        // ② 缓存已经太小了（当年从 MSIX 壳 exe 里只抽到 32×32 那档），重抽一次。
        //    这条是自愈：老版本写进磁盘的烂图标不用手工清，下次扫描自己换掉。
        if dst.exists() && !undersized(&dst) {
            continue;
        }
        if dst.exists() {
            let _ = fs::remove_file(&dst);
        }
        let p = Path::new(exe);
        if !p.exists() {
            continue;
        }
        // ③ MSIX 包自带 logo：直接拷（比抽壳 exe 准，也比壳 exe 那档大）
        if let Some(logo) = msix_logo(p) {
            if copy_logo(&logo, &dst).is_ok() {
                forget_cache(k);
                continue;
            }
        }
        // ④ 退回从 exe 抽
        if carries_own_icon(exe) {
            missing.push((k.clone(), exe.clone()));
        }
        // ④ 退回从 exe 抽
        if carries_own_icon(exe) {
            missing.push((k.clone(), exe.clone()));
        }
    }
    if missing.is_empty() {
        return;
    }
    let pairs: Vec<String> = missing
        .iter()
        .map(|(k, exe)| format!("{}|{}", safe_key(k), exe.replace('\'', "''")))
        .collect();
    let script = format!(
        r#"[Console]::OutputEncoding = [Text.Encoding]::UTF8
Add-Type -AssemblyName System.Drawing
# 32×32 的 ExtractAssociatedIcon 放到 28px 格子里勉强够，再大就糊——所以走 shell 的
# 系统图标列表（SHIL_JUMBO = 256×256），拿 exe 里真正那张大图。
# 注意：C# 段只返回 HICON，不引用 System.Drawing 类型——这台机器的 PowerShell 里
# System.Drawing.Icon 被转发到 System.Drawing.Common，Add-Type 引不到，编译直接失败。
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class BigIcon {{
  [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
  public struct SHFILEINFO {{
    public IntPtr hIcon; public int iIcon; public uint dwAttributes;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 260)] public string szDisplayName;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 80)] public string szTypeName;
  }}
  [DllImport("shell32.dll", CharSet = CharSet.Unicode)]
  public static extern IntPtr SHGetFileInfo(string path, uint attr, ref SHFILEINFO sfi, uint cb, uint flags);
  [DllImport("shell32.dll")]
  public static extern int SHGetImageList(int iImageList, ref Guid riid,
    [MarshalAs(UnmanagedType.Interface)] out IImageList ppv);
  [ComImport, Guid("46EB5926-582E-4017-9FDF-E8998DAA0950"),
   InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
  public interface IImageList {{
    [PreserveSig] int Add(IntPtr hbm, IntPtr hbmMask, ref int pi);
    [PreserveSig] int ReplaceIcon(int i, IntPtr hicon, ref int pi);
    [PreserveSig] int SetOverlayImage(int iImage, int iOverlay);
    [PreserveSig] int Replace(int i, IntPtr hbm, IntPtr hbmMask);
    [PreserveSig] int AddMasked(IntPtr hbm, int crMask, ref int pi);
    [PreserveSig] int Draw(IntPtr pimldp);
    [PreserveSig] int Remove(int i);
    [PreserveSig] int GetIcon(int i, int flags, ref IntPtr picon);
  }}
  // size: 0=大(32) 1=小(16) 2=超大(48) 4=JUMBO(256)
  public static IntPtr GetHandle(string path, int size) {{
    SHFILEINFO sfi = new SHFILEINFO();
    // SHGFI_SYSICONINDEX = 0x4000（不是 0x1000，那是 SHGFI_ICONLOCATION——
    // 写错就永远拿到 iIcon=0，即系统列表第 0 项那张空白文档图）
    IntPtr r = SHGetFileInfo(path, 0, ref sfi, (uint)Marshal.SizeOf(sfi), 0x4000);
    if (r == IntPtr.Zero) return IntPtr.Zero;
    Guid iid = new Guid("46EB5926-582E-4017-9FDF-E8998DAA0950");
    IImageList list;
    if (SHGetImageList(size, ref iid, out list) != 0 || list == null) return IntPtr.Zero;
    IntPtr h = IntPtr.Zero;
    if (list.GetIcon(sfi.iIcon, 1, ref h) != 0) return IntPtr.Zero;
    return h;
  }}
}}
'@
$lines = @'
{}
'@ -split "`n"
foreach ($l in $lines) {{
  if (-not $l.Trim()) {{ continue }}
  $i = $l.IndexOf('|')
  $key = $l.Substring(0, $i); $exe = $l.Substring($i + 1)
  try {{
    $h = [BigIcon]::GetHandle($exe, 4)
    if ($h -ne [IntPtr]::Zero) {{ $ico = [System.Drawing.Icon]::FromHandle($h) }}
    else {{ $ico = [System.Drawing.Icon]::ExtractAssociatedIcon($exe) }}
    if ($ico -ne $null) {{
      $bmp = $ico.ToBitmap()
      $out = Join-Path '{}' ($key + '.png')
      $bmp.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)
      $bmp.Dispose(); $ico.Dispose()
    }}
  }} catch {{ }}
}}"#,
        pairs.join("\n"),
        icon_dir().to_string_lossy().replace('\'', "''")
    );
    let mut c = std::process::Command::new("powershell");
    c.args(["-NoProfile", "-Command", &script]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = c.output();
}

/// 取某条目的图标 data URI（没有就返回 None，前端回退字母方块）
pub fn get(key: &str) -> Option<String> {
    let png = file_for(key);
    if !png.exists() {
        return None;
    }
    let mut g = match cache().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if let Some(hit) = g.get(key) {
        return Some(hit.clone());
    }
    let uri = data_uri(&png)?;
    g.insert(key.to_string(), uri.clone());
    Some(uri)
}

/// 重抽之后把内存里那份旧 data URI 扔掉，不然这一轮运行里界面还拿着那张烂图。
fn forget_cache(key: &str) {
    let mut g = match cache().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    g.remove(key);
}

#[cfg(test)]
mod tests {
    use super::{carries_own_icon, copy_logo, msix_logo, png_edge, undersized};
    use std::fs;
    use std::path::Path;

    #[test]
    fn script_launchers_are_not_worth_extracting() {
        // CLI 的入口常常是个 .cmd 包装，抽出来只会是系统那张空白文档图
        for p in [
            r"C:\Users\Example\AppData\Roaming\npm\codex.cmd",
            r"C:\Users\Example\AppData\Roaming\npm\claude.CMD",
            r"C:\tools\run.bat",
            r"C:\tools\probe.ps1",
        ] {
            assert!(!carries_own_icon(p), "{p} 是脚本，不该去抽图标");
        }
        for p in [
            r"D:\Apps\WorkBuddy.exe",
            r"C:\x\Qoder CN.EXE",
            r"C:\Users\me\Desktop\TRAE.lnk",
            r"D:\Apps\node_modules\opencode\bin\opencode",
        ] {
            assert!(carries_own_icon(p), "{p} 有真实图标可抽");
        }
    }

    /// 缓存里存了一张 32×32 的小图（MSIX 壳 exe 抽出来的就是这一档），
    /// 贴在 28px 的侧栏格子里再被高 DPI 拉一次就是一团糊——这种缓存必须认出来并重抽。
    #[test]
    fn tiny_cached_icons_are_treated_as_broken() {
        let dir = std::env::temp_dir().join("cockpit-icon-edge-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let big = dir.join("big.png");
        write_png(&big, 256);
        assert!(!undersized(&big), "256 的图不算小");

        let small = dir.join("small.png");
        write_png(&small, 32);
        assert!(undersized(&small), "32 的图该被判成废图");

        let mid = dir.join("mid.png");
        write_png(&mid, 64);
        assert!(!undersized(&mid), "64 正好够用，不算小");

        let not_png = dir.join("broken.png");
        fs::write(&not_png, b"not a png at all").unwrap();
        assert!(!undersized(&not_png), "读不出尺寸就不该乱判（按废图重抽代价大）");

        let _ = fs::remove_dir_all(&dir);
    }

    /// MSIX 包的 assets 里同时躺着 16/32/48/…/256 和 scale-125/150/200/400 若干档，
    /// 早先写死的名字列表先命中的常常是 32 或 88 的小图。必须挑短边最大的那张。
    #[test]
    fn msix_logo_picks_the_largest_asset_not_the_first_by_name() {
        let dir = std::env::temp_dir().join("cockpit-msix-logo-test");
        let _ = fs::remove_dir_all(&dir);
        let assets = dir.join("assets");
        fs::create_dir_all(&assets).unwrap();
        fs::create_dir_all(dir.join("app")).unwrap();
        let exe = dir.join("app").join("ChatGPT.exe");
        fs::write(&exe, b"stub").unwrap();

        write_png(&assets.join("Square44x44Logo.targetsize-32_altform-lightunplated.png"), 32);
        write_png(&assets.join("Square44x44Logo.targetsize-256_altform-lightunplated.png"), 256);
        write_png(&assets.join("Square150x150Logo.png"), 300);
        // 启动图/横图不是图标，别被选中
        write_png(&assets.join("SplashScreen.scale-200.png"), 1240);
        write_png(&assets.join("Wide310x150Logo.scale-200.png"), 620);

        let picked = msix_logo(&exe).expect("包里明明有 logo");
        let name = picked.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            name.starts_with("Square150x150Logo"),
            "应挑最大的那张（300px），却挑了 {name}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// WindowsApps 下的文件带 EFS 加密标记，`fs::copy` 会报 6000「无法加密指定的文件」。
    /// 这里要保证 copy_logo 走的是「读字节再写」这条路，MSIX 包的 logo 才拷得进来。
    #[test]
    fn copy_logo_reads_bytes_instead_of_relying_on_fs_copy() {
        let dir = std::env::temp_dir().join("cockpit-copy-logo-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.png");
        let dst = dir.join("dst.png");
        write_png(&src, 256);

        copy_logo(&src, &dst).expect("拷 logo 应该成功");
        assert_eq!(fs::read(&dst).unwrap(), fs::read(&src).unwrap(), "内容应逐字节一致");
        assert_eq!(png_edge(&dst), Some(256));

        let _ = fs::remove_dir_all(&dir);
    }

    /// 造一张指定边长的 PNG（只写文件头，够 png_edge 读尺寸用）。
    fn write_png(path: &Path, edge: u32) {
        let mut data = Vec::new();
        data.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        data.extend_from_slice(&13u32.to_be_bytes()); // IHDR 长度
        data.extend_from_slice(b"IHDR");
        data.extend_from_slice(&edge.to_be_bytes());
        data.extend_from_slice(&edge.to_be_bytes());
        data.extend_from_slice(&[8u8, 6, 0, 0, 0]); // 8bit RGBA
        data.extend_from_slice(&0u32.to_be_bytes()); // CRC（读尺寸用不到）
        fs::write(path, &data).unwrap();
    }
}
