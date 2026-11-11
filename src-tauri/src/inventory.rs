// 本机 Agent 安装现状盘点：三路查痕迹（注册表卸载表 / 快捷方式 / 安装根目录 + npm 全局），
// 与上次快照比对报「新装 / 卸载 / 内置路径失效」，并拿 ~/.agents/catalog.json 里预存的索引信息补口径。
use crate::discover::{is_ai_in, norm};
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
  $k = "$kind|$name|$path".ToLower()
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
    Emit 'reg' $n $_.DisplayIcon
    Emit 'reg' $n ($_.InstallLocation.TrimEnd('\'))
  }
}
$sh = New-Object -ComObject WScript.Shell
$lnkRoots = @("$env:APPDATA\Microsoft\Windows\Start Menu\Programs",
              "$env:ProgramData\Microsoft\Windows\Start Menu\Programs",
              "$env:PUBLIC\Desktop", "$env:USERPROFILE\Desktop", [Environment]::GetFolderPath('Desktop'))
foreach ($d in $lnkRoots) {
  Get-ChildItem -Path $d -Filter *.lnk -Recurse | ForEach-Object {
    try { Emit 'lnk' $_.BaseName ($sh.CreateShortcut($_.FullName).TargetPath) } catch {}
  }
}
$extraRoots = @($env:COCKPIT_SCAN_ROOTS | ConvertFrom-Json)
foreach ($d in (@("$env:LOCALAPPDATA\Programs", "$env:LOCALAPPDATA\LenovoSoftstore\Install", "$env:SystemDrive\LenovoSoftstore\Install", $env:ProgramFiles, ${env:ProgramFiles(x86)}) + $extraRoots)) {
  if (-not $d -or -not (Test-Path -LiteralPath $d -PathType Container)) { continue }
  Get-ChildItem -LiteralPath $d -Directory | ForEach-Object {
    Emit 'dir' $_.Name $_.FullName
  }
  if ($extraRoots -contains $d) {
    Get-ChildItem -LiteralPath $d -Filter *.exe -File | ForEach-Object { Emit 'dir' $_.BaseName $_.FullName }
  }
}
Get-ChildItem "$env:APPDATA\npm" -Filter *.cmd | ForEach-Object { Emit 'npm' $_.BaseName $_.FullName }
foreach ($name in @('codex','claude','gemini','qwen','opencode','aider','kimi','iflow')) {
  Get-Command -Name $name -CommandType Application -All | ForEach-Object { Emit 'cli' $name $_.Source }
}"#.to_string()
}

/// 注册表 DisplayIcon 常写成 `"C:\\...\\Foo.exe",0`；安装位置又可能是目录。
/// 只接受真实存在的可执行文件，后面的收养/收录都靠它。
pub(crate) fn trace_exe(raw: &str) -> Option<PathBuf> {
    let mut s = raw.trim().trim_matches('"').trim().to_string();
    if let Some((head, tail)) = s.rsplit_once(',') {
        if tail.trim().parse::<i32>().is_ok() {
            s = head.trim().trim_matches('"').to_string();
        }
    }
    let p = PathBuf::from(s);
    if !p.is_file() {
        return None;
    }
    let ext = p
        .extension()
        .and_then(|x| x.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(ext.as_str(), "exe" | "cmd" | "bat").then_some(p)
}

struct Trace {
    kind: String,
    display: String,
    path: String,
}
type Inventory = BTreeMap<String, Vec<Trace>>;

fn probe() -> Inventory {
    let roots = crate::discover::ai_install_roots();
    let mut c = std::process::Command::new("powershell");
    c.env("COCKPIT_SCAN_ROOTS", json!(roots).to_string());
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
    let mut map = Inventory::new();
    for line in text.lines() {
        let mut it = line.splitn(3, '|');
        let (kind, name, path) = (it.next().unwrap_or(""), it.next().unwrap_or(""), it.next().unwrap_or(""));
        if name.is_empty() || path.is_empty() {
            continue;
        }
        // 只留像 AI 客户端的条目，否则一台机器能列出几百个软件
        if !is_ai_in(name, path, &roots) {
            continue;
        }
        let key = norm(name);
        // Only use uniquely identified product executables from directory traces.
        // A registry InstallLocation alone is not an executable or evidence that
        // the first *.exe in it is the program (it is often the uninstaller).
        let executable = trace_exe(path).or_else(|| directory_exe(name, path));
        let normalized = executable.as_ref().map(|p| p.to_string_lossy().into_owned());
        let display = if kind == "cli" {
            reg::apps().into_iter().find(|a| a.kind == "cli" && norm(&a.id) == key)
                .map(|a| a.name).unwrap_or_else(|| name.to_owned())
        } else { name.to_owned() };
        map.entry(key).or_default().push(Trace {
            kind: kind.into(), display, path: normalized.unwrap_or_else(|| path.into()),
        });
    }
    map
}

fn directory_exe(name: &str, raw: &str) -> Option<PathBuf> {
    let root = PathBuf::from(raw.trim().trim_matches('"'));
    if !root.is_dir() || reg::is_reparse_point(&root) { return None; }
    let mut stems = vec![norm(name)];
    if let Some(leaf) = root.file_name().and_then(|s| s.to_str()) { stems.push(norm(leaf)); }
    for app in reg::apps() {
        if stems.contains(&norm(&app.name)) || stems.contains(&norm(&app.id)) {
            let leaf = reg::exe_name(&app);
            if let Some(stem) = std::path::Path::new(&leaf).file_stem().and_then(|s| s.to_str()) {
                stems.push(norm(stem));
            }
        }
    }
    // Breadth-first: prefer the top-level official entry over an old version.
    // Both the number of filesystem entries and the depth are bounded.
    let mut dirs = std::collections::VecDeque::from([(root, 0)]);
    let mut budget = 2000;
    while let Some((dir, depth)) = dirs.pop_front() {
        let Ok(entries) = fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            if budget == 0 { return None; }
            budget -= 1;
            let p = entry.path();
            if reg::is_reparse_point(&p) { continue; }
            if p.is_dir() {
                if depth < 2 { dirs.push_back((p, depth + 1)); }
            } else if p.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
                && p.file_stem().is_some_and(|s| stems.contains(&norm(&s.to_string_lossy())))
            {
                return Some(p);
            }
        }
    }
    None
}

/// 内置清单里已经指不到可执行文件的条目，并顺手在痕迹里找它搬去哪了
fn stale_paths(found: &Inventory) -> Vec<Value> {
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
            for trace in list {
                if std::path::Path::new(&trace.path).file_name().is_some_and(|n| n.eq_ignore_ascii_case(&want)) {
                    moved.push(trace.path.clone());
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

/// 内置条目的路径在别的机器上失效，但痕迹里找到了真身：写成本机路径覆盖。
/// 这样不用改共享的内置清单，也不用再生成一张重复卡。
fn candidates(found: &Inventory) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for traces in found.values() {
        for t in traces {
            let Some(p) = trace_exe(&t.path) else { continue };
            let path = p.to_string_lossy().into_owned();
            if seen.insert(crate::discover::norm_path(&path)) {
                out.push((t.display.clone(), path));
            }
        }
    }
    out
}

pub(crate) fn discovery_candidates() -> Vec<(String, String)> {
    candidates(&probe())
}

fn collect_detected(found: &Inventory) -> (Vec<Value>, Vec<Value>) {
    let installed: Vec<_> = reg::all_apps_raw().into_iter()
        .filter(|a| reg::resolve_exe(a).is_some()).collect();
    let mut known = reg::claimed_names_for(&installed, &|_| true);
    let mut repaired = Vec::new();
    let mut collected = Vec::new();
    for (display, path) in candidates(found) {
        if crate::discover::claimed_by_manual(&known, &display, &path) { continue; }
        if let Some(id) = reg::adopt_exe_for_candidate(&display, &path) {
            repaired.push(json!({"id": id, "name": display, "path": path}));
        } else {
            let app = reg::make_custom(&display, &path);
            reg::save_custom(&app);
            collected.push(json!({"name": display, "path": path, "app": app.id, "adopted": false}));
        }
        known.insert(norm(&display));
        known.insert(crate::discover::norm_path(&path));
        if let Some(leaf) = std::path::Path::new(&path).file_name().and_then(|s| s.to_str()) {
            if !crate::discover::generic_exe(leaf) { known.insert(norm(leaf)); }
        }
    }
    (repaired, collected)
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
        .map(|(k, v)| (k.clone(), v.iter().map(|t| format!("{}:{}", t.kind, t.path)).collect()))
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
    let stale = stale_paths(&found);
    // 先修内置失效路径，再收新程序：收的时候 known 集合已经能看到修好的条目
    let (repaired, collected) = collect_detected(&found);
    json!({
        "ok": true,
        "detected": now.len(),
        "new": new, "gone": gone,
        "first_run": prev.is_empty(),
        "stale_paths": stale,
        "repaired": repaired,
        "collected": collected,
        "catalog": { "loaded": in_catalog, "path": home().join(".agents").join("catalog.json").to_string_lossy() },
        "detail": now,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_exe_strips_icon_index_and_requires_executable() {
        let tmp = std::env::temp_dir().join(format!("cockpit-trace-exe-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let exe = tmp.join("Foo.exe");
        fs::write(&exe, b"x").unwrap();
        assert_eq!(trace_exe(&format!("{},0", exe.display())), Some(exe.clone()));
        assert_eq!(trace_exe(&format!("\"{}\"", exe.display())), Some(exe.clone()));
        let cmd = tmp.join("bar.cmd");
        fs::write(&cmd, b"x").unwrap();
        assert_eq!(trace_exe(&cmd.to_string_lossy()), Some(cmd));
        assert_eq!(trace_exe(&tmp.to_string_lossy()), None, "目录不是可执行文件");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn directory_exe_prefers_named_entry_and_ignores_uninstaller() {
        let root = std::env::temp_dir().join(format!("cockpit-directory-exe-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("AStudio")).unwrap();
        fs::write(root.join("Uninstall AStudio.exe"), b"x").unwrap();
        fs::write(root.join("AStudio").join("AStudio.exe"), b"x").unwrap();
        assert_eq!(
            directory_exe("AStudio", &root.to_string_lossy()),
            Some(root.join("AStudio").join("AStudio.exe"))
        );
        let _ = fs::remove_dir_all(&root);
    }
}
