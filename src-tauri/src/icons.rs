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
fn msix_logo(exe: &Path) -> Option<PathBuf> {
    let root = exe.parent()?.parent()?; // <包根>/app/ChatGPT.exe → <包根>
    let assets = root.join("assets");
    for name in [
        // 256 的浅色版：透明底、深色线稿，浅色主题下最好认
        "Square44x44Logo.targetsize-256_altform-lightunplated.png",
        "Square44x44Logo.scale-200.png",
        "Square44x44Logo.png",
        "Square150x150Logo.png",
    ] {
        let p = assets.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// 用户自己指定的图标：`icons/custom/<产品id>.png`。放进去就生效，
/// 优先于包内 logo 和 exe 抽取——有些产品抽出来/自带的都不是想要的那张。
fn custom_icon(app_id: &str) -> Option<PathBuf> {
    let p = icon_dir().join("custom").join(format!("{app_id}.png"));
    p.is_file().then_some(p)
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
        if dst.exists() {
            continue;
        }
        let p = Path::new(exe);
        if !p.exists() {
            continue;
        }
        // ② MSIX 包自带 logo：直接拷（比抽壳 exe 准）
        if let Some(logo) = msix_logo(p) {
            if fs::copy(&logo, &dst).is_ok() {
                continue;
            }
        }
        // ③ 退回从 exe 抽
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

#[cfg(test)]
mod tests {
    use super::carries_own_icon;

    #[test]
    fn script_launchers_are_not_worth_extracting() {
        // CLI 的入口常常是个 .cmd 包装，抽出来只会是系统那张空白文档图
        for p in [
            r"D:\npm-global\codex.cmd",
            r"D:\npm-global\claude.CMD",
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
}
