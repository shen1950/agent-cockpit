// 本机 Agent 安装现状盘点：三路查痕迹（注册表卸载表 / 快捷方式 / 安装根目录 + npm 全局），
// 与上次快照比对报「新装 / 卸载 / 内置路径失效」，并拿 ~/.agents/catalog.json 里预存的索引信息补口径。
use crate::discover::{is_ai, norm};
use crate::registry::{self as reg, home};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn snapshot_file() -> PathBuf {
    reg::base_dir().join("inventory.json")
}

/// 一次 PowerShell 取回所有痕迹，避免多次冷启动解释器（每次约 0.6s）
fn probe_script() -> String {
    r#"[Console]::OutputEncoding = [Text.Encoding]::UTF8
$ErrorActionPreference='SilentlyContinue'
$seen = @{}
function Emit($kind, $name, $path) {
  if (-not $path) { return }
  $k = $path.ToLower()
  if ($seen.ContainsKey($k)) { return }
  $seen[$k] = 1
  "{0}|{1}|{2}" -f $kind, $name, $path
}
$roots = @('HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*',
           'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*',
           'HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*',
           'HKCU:\Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\PackageRepository\Packages\*')
foreach ($r in $roots) {
  Get-ItemProperty $r | ForEach-Object {
    $n = $_.DisplayName; if (-not $n) { $n = $_.PSChildName }
    Emit 'reg' $n ($_.InstallLocation.TrimEnd('\'))
    Emit 'reg' $n $_.DisplayIcon
  }
}
$sh = New-Object -ComObject WScript.Shell
$lnkRoots = @("$env:APPDATA\Microsoft\Windows\Start Menu\Programs",
              "$env:ProgramData\Microsoft\Windows\Start Menu\Programs",
              "$env:PUBLIC\Desktop", "$env:USERPROFILE\Desktop")
foreach ($d in $lnkRoots) {
  Get-ChildItem -Path $d -Filter *.lnk -Recurse | ForEach-Object {
    try { Emit 'lnk' $_.BaseName ($sh.CreateShortcut($_.FullName).TargetPath) } catch {}
  }
}
foreach ($d in @("$env:LOCALAPPDATA\Programs", 'D:\Artificial_Intelligence', 'D:\LenovoSoftstore\Install', $env:ProgramFiles, ${env:ProgramFiles(x86)})) {
  Get-ChildItem -Path $d -Directory | ForEach-Object {
    $exe = Get-ChildItem -Path $_.FullName -Filter *.exe -Depth 1 | Select-Object -First 1
    if ($exe) { Emit 'dir' $_.Name $exe.FullName }
  }
}
Get-ChildItem 'D:\npm-global' -Filter *.cmd | ForEach-Object { Emit 'npm' $_.BaseName $_.FullName }"#.to_string()
}

fn probe() -> BTreeMap<String, Vec<(String, String)>> {
    let mut c = std::process::Command::new("powershell");
    c.args(["-NoProfile", "-Command", &probe_script()]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    let out = match c.output() {
        Ok(o) => o,
        Err(_) => return BTreeMap::new(),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut map: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for line in text.lines() {
        let mut it = line.splitn(3, '|');
        let (kind, name, path) = (it.next().unwrap_or(""), it.next().unwrap_or(""), it.next().unwrap_or(""));
        if name.is_empty() || path.is_empty() {
            continue;
        }
        // 只留像 AI 客户端的条目，否则一台机器能列出几百个软件
        if !is_ai(name, path) {
            continue;
        }
        let key = norm(name);
        map.entry(key).or_default().push((kind.into(), path.into()));
    }
    map
}

/// 内置清单里已经指不到可执行文件的条目，并顺手在痕迹里找它搬去哪了
fn stale_paths(found: &BTreeMap<String, Vec<(String, String)>>) -> Vec<Value> {
    let mut out = Vec::new();
    for a in reg::apps() {
        if reg::resolve_exe(&a).is_some() {
            continue;
        }
        let want = reg::exe_name(&a).to_lowercase();
        let mut moved: Vec<String> = Vec::new();
        for (k, list) in found {
            if !k.contains(&norm(&want.replace(".exe", ""))) && !k.contains(&norm(&a.id)) {
                continue;
            }
            for (_, p) in list {
                if p.to_lowercase().ends_with(&want) {
                    moved.push(p.clone());
                }
            }
        }
        moved.sort();
        moved.dedup();
        out.push(json!({
            "id": a.id, "name": a.name, "recorded": a.exes,
            "found_at": moved, "has_profile": reg::profile_dir(&a.id).map(|p| p.exists()).unwrap_or(false),
        }));
    }
    out
}

fn catalog() -> Value {
    let p = home().join(".agents").join("catalog.json");
    match fs::read_to_string(&p) {
        Ok(s) => serde_json::from_str(&s).unwrap_or(Value::Null),
        Err(_) => Value::Null,
    }
}

/// 扫描并落快照：第一次跑只有 "current"，之后每次都能报出新装/卸载
pub fn scan() -> Value {
    let found = probe();
    let now: BTreeMap<String, Vec<String>> = found
        .iter()
        .map(|(k, v)| (k.clone(), v.iter().map(|(kind, p)| format!("{kind}:{p}")).collect()))
        .collect();
    let prev: BTreeMap<String, Vec<String>> = fs::read_to_string(snapshot_file())
        .ok()
        .and_then(|s| serde_json::from_str(&json!({"m": s})["m"].to_string().replace("\\\"", "\"")).ok())
        .or_else(|| {
            fs::read_to_string(snapshot_file())
                .ok()
                .and_then(|s| serde_json::from_str::<BTreeMap<String, Vec<String>>>(&s).ok())
        })
        .unwrap_or_default();

    let new: Vec<&String> = now.keys().filter(|k| !prev.contains_key(*k)).collect();
    let gone: Vec<&String> = prev.keys().filter(|k| !now.contains_key(*k)).collect();

    if let Some(dir) = snapshot_file().parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::write(
        snapshot_file(),
        serde_json::to_string_pretty(&now).unwrap_or_default(),
    );

    let cat = catalog();
    let in_catalog = cat
        .get("agents")
        .and_then(|a| a.as_array())
        .map(|v| v.len())
        .unwrap_or(0);
    json!({
        "ok": true,
        "detected": now.len(),
        "new": new, "gone": gone,
        "first_run": prev.is_empty(),
        "stale_paths": stale_paths(&found),
        "catalog": { "loaded": in_catalog, "path": home().join(".agents").join("catalog.json").to_string_lossy() },
        "detail": now,
    })
}
