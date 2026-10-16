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

/// 一次性把所有缺失的图标抽出来（单个 PowerShell 进程，避免逐个启动）
pub fn refresh(items: &[(String, String)]) {
    let _ = fs::create_dir_all(icon_dir());
    let missing: Vec<&(String, String)> = items
        .iter()
        .filter(|(k, exe)| {
            !file_for(k).exists() && Path::new(exe).exists() && carries_own_icon(exe)
        })
        .collect();
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
$lines = @'
{}
'@ -split "`n"
foreach ($l in $lines) {{
  if (-not $l.Trim()) {{ continue }}
  $i = $l.IndexOf('|')
  $key = $l.Substring(0, $i); $exe = $l.Substring($i + 1)
  try {{
    $ico = [System.Drawing.Icon]::ExtractAssociatedIcon($exe)
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
