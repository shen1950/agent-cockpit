// 便携 exe 的 Windows 外壳集成：桌面快捷方式 / 开始菜单项的查、建、删。
// 为什么单独一个模块：.lnk 只能借 PowerShell + WScript.Shell COM 生成，是重型子进程动作，
// 和 Agent 台账没有任何关系；塞进 commands.rs 会让它更难维护，而且那边同时有别的会话在改。
//
// 三条铁律（都是这机器上踩过的坑）：
// 1. 所有路径在 Rust 侧算成绝对路径才交给子进程——GUI 被别的程序唤起后派生的命令读不到
//    %APPDATA%，寄希望于子进程展开环境变量会静默退化成相对路径。
// 2. PowerShell 一律 -EncodedCommand（base64 UTF-16LE）：快捷方式名带中文，不能拼进命令行。
// 3. 删除前必须校验 .lnk 的 TargetPath 确实指向本程序，且先备份再删；名字对不上别的桌面项一律不碰。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 快捷方式落盘用的名字。删的时候按这个精确匹配，不做任何通配。
pub const LINK_NAME: &str = "Agent 驾驶舱";

/// 开始菜单里的第二个入口，只为了让英文搜索也能命中：程序名（窗口标题、exe 的
/// FileDescription/ProductName、快捷方式名）统一是「Agent 驾驶舱」，而仓库与目录叫
/// AgentCockpit，Windows 应用搜索按快捷方式显示名命中，搜 agentcockpit 会落空。
/// 用户 2026-10-11 明确要这条，所以放在同一个子目录里、指向同一个 exe。
pub const LINK_ALIAS: &str = "AgentCockpit";

/// 英文名那条落在中文名那个子目录里，清理时整目录好认
fn start_alias_of(start: &Path) -> PathBuf {
    start.with_file_name(format!("{LINK_ALIAS}.lnk"))
}

fn ok(msg: impl Into<String>) -> Value {
    json!({ "ok": true, "msg": msg.into() })
}
fn err(msg: impl Into<String>) -> Value {
    json!({ "ok": false, "msg": msg.into() })
}

/// 展开 REG_EXPAND_SZ 里的 %VAR%。纯函数：变量怎么解析由调用方给 lookup，
/// 认不出/查不到的占位符原样保留，让上层判断结果是否可用，绝不悄悄拼出半截路径。
pub fn expand_vars(raw: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let b = raw.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if let Some(j) = b[i + 1..].iter().position(|&c| c == b'%') {
                let name = &raw[i + 1..i + 1 + j];
                if !name.is_empty() {
                    if let Some(v) = lookup(name) {
                        out.push_str(&v);
                        i = i + 2 + j;
                        continue;
                    }
                }
            }
        }
        let ch = raw[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// 归一化路径用于比较：小写 + 分隔符统一 + 去首尾引号/空白。
/// Windows 注册表、WScript 返回值、current_exe 三者的大小写和斜杠方向都不保证一致。
pub fn norm_path(p: &str) -> String {
    p.trim()
        .trim_matches('"')
        .replace('/', "\\")
        .to_lowercase()
}

/// 是否盘符绝对路径或 UNC。所有要塞进 PowerShell 脚本的路径先过这道闸：
/// 一是挡注入（配合 ps_quote 的单引号加倍），二是万一上面的解析退化成相对路径，
/// 在这里当场报错，而不是把 .lnk 写到不知哪里去。
pub fn is_abs_path(p: &str) -> bool {
    let b = p.as_bytes();
    (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'))
        || p.starts_with("\\\\")
}

/// 判定一个 .lnk 的 TargetPath 是不是本程序。只认完全一致的路径，
/// 不做前缀/目录匹配——否则 "Agent 驾驶舱 2.exe" 这种邻居会被当成自己人。
pub fn is_our_target(target: &str, exe: &Path) -> bool {
    let t = norm_path(target);
    !t.is_empty() && t == norm_path(&exe.to_string_lossy())
}

/// PowerShell 单引号字符串转义：内部单引号翻倍，别的字符在单引号串里都是字面量。
pub fn ps_quote(p: &str) -> String {
    p.replace('\'', "''")
}

/// Desktop / Programs 两个外壳目录 + 回退（注册表读不到时按最常见位置猜）。
/// 纯函数：home/appdata 与额外变量的查法都由参数注入，单测里不碰真实环境。
pub fn resolve_lnk_paths(
    desktop_raw: Option<&str>,
    programs_raw: Option<&str>,
    home: &Path,
    appdata: &Path,
    extra: &dyn Fn(&str) -> Option<String>,
) -> (PathBuf, PathBuf) {
    let lookup = |n: &str| match n.to_ascii_uppercase().as_str() {
        "USERPROFILE" | "HOME" => Some(home.to_string_lossy().into_owned()),
        "APPDATA" => Some(appdata.to_string_lossy().into_owned()),
        _ => extra(n),
    };
    let dir = |raw: Option<&str>, fallback: PathBuf| -> PathBuf {
        match raw.map(str::trim).filter(|s| !s.is_empty()) {
            Some(r) => {
                let exp = expand_vars(r, &lookup);
                // 展开后仍是相对路径 = 变量没认出来（比如 OneDrive 占位符），
                // 用回退位置，绝不把相对路径交给子进程
                if is_abs_path(&exp) {
                    PathBuf::from(exp)
                } else {
                    fallback
                }
            }
            None => fallback,
        }
    };
    let desktop = dir(
        desktop_raw,
        home.join("Desktop"),
    );
    let programs = dir(
        programs_raw,
        appdata.join("Microsoft").join("Windows").join("Start Menu").join("Programs"),
    );
    (
        desktop.join(format!("{LINK_NAME}.lnk")),
        // 开始菜单按惯例放一个以程序名命名的子目录，卸载/清理时也好认
        programs.join(LINK_NAME).join(format!("{LINK_NAME}.lnk")),
    )
}

/// 本进程能看到的真实变量（注册表值里出现 USERPROFILE/APPDATA 之外的占位符时用）
fn env_lookup(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn home_dir() -> PathBuf {
    crate::registry::home()
}

fn appdata_dir(home: &Path) -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join("AppData").join("Roaming"))
}

/// base64（标准表 + padding），不引新依赖：整份脚本要编码成 UTF-16LE 喂 -EncodedCommand，
/// 中文快捷方式名只有这样才进得了命令行——直接 -Command 拼参数在 GBK 控制台上会变问号。
fn b64_utf16le(script: &str) -> String {
    const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(TBL[(n >> 18) as usize & 63] as char);
        out.push(TBL[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TBL[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TBL[n as usize & 63] as char } else { '=' });
    }
    out
}

fn run_ps(script: &str) -> Result<String, String> {
    let mut c = std::process::Command::new("powershell");
    c.args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &b64_utf16le(script)]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // 同 discover.rs：从隐藏控制台启动的 GUI 会继承 STARTF_USESHOWWINDOW，
        // 不加这个标志子 PowerShell 可能把窗口亮出来
        c.creation_flags(CREATE_NO_WINDOW);
    }
    let out = c.output().map_err(|e| format!("无法启动 PowerShell：{e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// 读 User Shell Folders 的 Desktop/Programs 原始值（不展开环境变量——留给我们自己控制展开，
/// 顺带 OneDrive 重定向桌面就藏在这里，硬拼 %USERPROFILE%\Desktop 会指错地方）
fn read_shell_folders() -> (Option<String>, Option<String>) {
    let script = "[Console]::OutputEncoding = [Text.Encoding]::UTF8\n\
                  $k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\User Shell Folders')\n\
                  if (-not $k) { exit }\n\
                  $d = $k.GetValue('Desktop', '', 'DoNotExpandEnvironmentNames')\n\
                  $p = $k.GetValue('Programs', '', 'DoNotExpandEnvironmentNames')\n\
                  \"D=$d\"\n\"P=$p\"";
    match run_ps(script) {
        Ok(o) => {
            let mut d = None;
            let mut p = None;
            for line in o.lines() {
                if let Some(v) = line.strip_prefix("D=") {
                    d = Some(v.trim().to_string());
                } else if let Some(v) = line.strip_prefix("P=") {
                    p = Some(v.trim().to_string());
                }
            }
            (d, p)
        }
        // 注册表读不到不视为错误：回退默认位置，界面上照旧显示解析出的绝对路径
        Err(_) => (None, None),
    }
}

/// 读若干 .lnk 的 TargetPath（一行一个；不存在/读不出为空串）
fn read_targets(paths: &[PathBuf]) -> Result<Vec<String>, String> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let list = paths
        .iter()
        .map(|p| format!("'{}'", ps_quote(&p.to_string_lossy())))
        .collect::<Vec<_>>()
        .join(", ");
    let script = format!(
        "[Console]::OutputEncoding = [Text.Encoding]::UTF8\n\
         $sh = New-Object -ComObject WScript.Shell\n\
         foreach ($p in @({list})) {{ $t = ''; if (Test-Path -LiteralPath $p) {{ try {{ $t = $sh.CreateShortcut($p).TargetPath }} catch {{}} }}; \"T=$t\" }}",
    );
    let out = run_ps(&script)?;
    let mut res: Vec<String> = Vec::new();
    for line in out.lines() {
        if let Some(v) = line.strip_prefix("T=") {
            res.push(v.trim().to_string());
        }
    }
    // 行数对不上（脚本被某台机器的策略坑了）就当全读不出，宁可显示"未知"也不猜
    res.resize(paths.len(), String::new());
    Ok(res)
}

struct Targets {
    exe: PathBuf,
    desktop: PathBuf,
    start: PathBuf,
}

fn gather() -> Result<Targets, String> {
    let exe = std::env::current_exe().map_err(|e| format!("取不到当前程序路径：{e}"))?;
    let home = home_dir();
    let appdata = appdata_dir(&home);
    let (d_raw, p_raw) = read_shell_folders();
    let (desktop, start) = resolve_lnk_paths(
        d_raw.as_deref(),
        p_raw.as_deref(),
        &home,
        &appdata,
        &env_lookup,
    );
    for p in [&desktop, &start, &exe] {
        if !is_abs_path(&p.to_string_lossy()) {
            return Err(format!("路径解析退化成相对路径（{p:?}），拒绝继续"));
        }
    }
    Ok(Targets { exe, desktop, start })
}

fn entry(lnk: &Path, exe: &Path, target: &str) -> Value {
    json!({
        "path": lnk.to_string_lossy(),
        "exists": lnk.exists(),
        "target": target,
        "ours": is_our_target(target, exe),
    })
}

/// 设置页打开时拉一次：两处 .lnk 的绝对路径、是否已存在、指向谁
pub fn status() -> Value {
    let t = match gather() {
        Ok(t) => t,
        Err(e) => return err(e),
    };
    let alias = start_alias_of(&t.start);
    let existing: Vec<PathBuf> = [&t.desktop, &t.start, &alias]
        .iter()
        .filter(|p| p.exists())
        .map(|p| (*p).clone())
        .collect();
    let targets = read_targets(&existing).unwrap_or_default();
    // 按 existing 顺序回填；不存在的位置给空 target
    let mut it = targets.into_iter();
    let mut next_for = |p: &Path| {
        if p.exists() {
            it.next().unwrap_or_default()
        } else {
            String::new()
        }
    };
    let d = next_for(&t.desktop);
    let s = next_for(&t.start);
    let a = next_for(&alias);
    json!({
        "ok": true,
        "msg": "",
        "exe": t.exe.to_string_lossy(),
        "desktop": entry(&t.desktop, &t.exe, if t.desktop.exists() { &d } else { "" }),
        "start": entry(&t.start, &t.exe, if t.start.exists() { &s } else { "" }),
        "start_alias": entry(&alias, &t.exe, if alias.exists() { &a } else { "" }),
    })
}

/// 首次运行（以及 exe 换了位置）后台补一条开始菜单项。
/// 便携程序不会自己进开始菜单，而「搜不到 → 打不开 → 没法进设置点按钮」是个死循环：
/// 用户就是这么撞上的。所以这一步不能等人按按钮，启动时自己做完。
/// 只在**那个位置没有文件**时写；同名但指着别的程序的 .lnk 一律不抢。
/// 起 PowerShell 是百毫秒级动作，放后台线程，绝不占启动路径。
pub fn ensure_start_menu() {
    let t = match gather() {
        Ok(t) => t,
        Err(_) => return,
    };
    let exe_s = t.exe.to_string_lossy().to_string();
    let lnk_s = t.start.to_string_lossy().to_string();
    let alias = start_alias_of(&t.start);
    // 记过账、路径没变、两条文件都还在 —— 一次子进程都不必起
    let mark = crate::settings::get();
    let m = mark.get("start_menu_link").and_then(|v| v.as_object());
    let unchanged = m
        .map(|o| {
            o.get("exe").and_then(|v| v.as_str()) == Some(exe_s.as_str())
                && o.get("lnk").and_then(|v| v.as_str()) == Some(lnk_s.as_str())
        })
        .unwrap_or(false);
    if unchanged && t.start.exists() && alias.exists() {
        return;
    }
    if t.start.exists() {
        // 已经有一个了：是我们的就只补记账，不是我们的（比如别的会话建的）就别动
        if let Some(target) = read_targets(&[t.start.clone()]).ok().and_then(|v| v.first().cloned()) {
            if !target.is_empty() && !is_our_target(&target, &t.exe) {
                return;
            }
        }
        if !alias.exists() {
            let _ = write_lnk(&t.exe, &alias);
        }
    } else if !create("startmenu")["ok"].as_bool().unwrap_or(false) {
        return;
    }
    crate::settings::update(&json!({ "start_menu_link": { "exe": exe_s, "lnk": lnk_s } }));
}

/// 写一个 .lnk。图标用 exe 自己（IconLocation "exe,0"），工作目录设成 exe 所在目录——
/// 便携程序的相对路径落点跟工作目录，不设的话程序在家目录下创建文件。
/// 返回 Err 只说这一条为什么没成，调用方决定要不要继续写下一条。
fn write_lnk(exe: &Path, lnk: &Path) -> Result<(), String> {
    if let Some(parent) = lnk.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return Err(format!("创建目录失败 {parent:?}：{e}"));
        }
    }
    let l = lnk.to_string_lossy();
    let e = exe.to_string_lossy();
    let w = exe.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    if !is_abs_path(&w) {
        return Err("exe 所在目录不是绝对路径，拒绝写入快捷方式".to_string());
    }
    let script = format!(
        "[Console]::OutputEncoding = [Text.Encoding]::UTF8\n\
         $sh = New-Object -ComObject WScript.Shell\n\
         $s = $sh.CreateShortcut('{}')\n\
         $s.TargetPath = '{}'\n\
         $s.WorkingDirectory = '{}'\n\
         $s.IconLocation = '{},0'\n\
         $s.Description = 'Agent Cockpit (portable)'\n\
         $s.Save()\n\
         if (Test-Path -LiteralPath '{}') {{ 'DONE' }} else {{ 'FAIL' }}",
        ps_quote(&l), ps_quote(&e), ps_quote(&w), ps_quote(&e), ps_quote(&l)
    );
    match run_ps(&script) {
        Ok(o) if o.lines().any(|x| x.trim() == "DONE") => Ok(()),
        Ok(_) => Err(format!("PowerShell 未确认落盘（{l}）")),
        Err(x) => Err(x),
    }
}

/// 生成/覆盖快捷方式。开始菜单一次写两条：中文名的正式项 + 英文名的搜索别名。
pub fn create(kind: &str) -> Value {
    // 先验 kind 再 gather：gather 要起 PowerShell，非法请求不该为它付子进程的钱
    if !matches!(kind, "desktop" | "startmenu") {
        return err("未知快捷方式位置（desktop / startmenu）");
    }
    let t = match gather() {
        Ok(t) => t,
        Err(e) => return err(e),
    };
    let (label, links): (&str, Vec<PathBuf>) = if kind == "desktop" {
        ("桌面", vec![t.desktop.clone()])
    } else {
        ("开始菜单", vec![t.start.clone(), start_alias_of(&t.start)])
    };
    let mut done: Vec<String> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    for lnk in &links {
        match write_lnk(&t.exe, lnk) {
            Ok(()) => done.push(lnk.to_string_lossy().into_owned()),
            Err(e) => failed.push(format!("{}：{e}", lnk.display())),
        }
    }
    if done.is_empty() {
        return err(format!("{label}快捷方式创建失败：{}", failed.join("\n")));
    }
    let mut msg = format!("已在{label}创建快捷方式：\n{}", done.join("\n"));
    if !failed.is_empty() {
        msg.push_str(&format!("\n没成的：\n{}", failed.join("\n")));
    }
    msg.push_str(&format!("\n指向：{}", t.exe.to_string_lossy()));
    ok(msg)
}

/// 移除本程序创建的 .lnk。破坏性操作三重锁：
/// 必须带 confirm；文件名精确等于 LINK_NAME；TargetPath 必须指向当前 exe。
/// 删除前先复制到数据根 shell-link-backups/，报告里原样列出路径 + 校验到的 TargetPath。
pub fn remove(confirm: bool) -> Value {
    // 确认闸门放在 gather 之前：未确认时不该起 PowerShell，也不该在报告里给半截上下文
    if !confirm {
        return err("这会删掉桌面/开始菜单里本程序的快捷方式，确认后才执行");
    }
    let t = match gather() {
        Ok(t) => t,
        Err(e) => return err(e),
    };
    let candidates = [
        ("桌面", t.desktop.clone()),
        ("开始菜单", t.start.clone()),
        ("开始菜单（英文别名）", start_alias_of(&t.start)),
    ];
    let existing: Vec<PathBuf> = candidates
        .iter()
        .map(|(_, p)| p)
        .filter(|p| p.exists())
        .cloned()
        .collect();
    if existing.is_empty() {
        return ok("三处都没有本程序的快捷方式，什么都没删");
    }
    let targets = match read_targets(&existing) {
        Ok(v) => v,
        Err(e) => return err(format!("读取现有快捷方式失败，全部不动：{e}")),
    };
    let backup_dir = crate::registry::base_dir().join("shell-link-backups");
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let mut lines: Vec<String> = Vec::new();
    let mut removed = 0;
    let mut idx = 0;
    for (label, p) in &candidates {
        if !p.exists() {
            lines.push(format!("{label}：{p}（本来就没有，不动）", p = p.display()));
            continue;
        }
        let target = targets.get(idx).cloned().unwrap_or_default();
        idx += 1;
        if !is_our_target(&target, &t.exe) {
            // 名字撞车但指向别的程序——这正是不许按名字通配删的原因
            lines.push(format!(
                "{label}：拒绝删除 {}，它的目标不是本程序（{target}）",
                p.display()
            ));
            continue;
        }
        if let Err(e) = std::fs::create_dir_all(&backup_dir) {
            lines.push(format!("{label}：备份目录建不出来，未删除：{e}"));
            continue;
        }
        let bak = backup_dir.join(format!("{stamp}-{}", p.file_name().unwrap_or_default().to_string_lossy()));
        if let Err(e) = std::fs::copy(p, &bak) {
            lines.push(format!("{label}：备份失败，未删除：{e}"));
            continue;
        }
        match std::fs::remove_file(p) {
            Ok(_) => {
                removed += 1;
                lines.push(format!(
                    "{label}：已删除 {}（目标 {target}，已备份到 {}）",
                    p.display(),
                    bak.display()
                ));
            }
            Err(e) => lines.push(format!("{label}：删除失败：{e}")),
        }
    }
    let msg = lines.join("\n");
    if removed == 0 {
        // 一条都没删成（要么没有要么全被拒）——不算失败，但界面得看清发生了什么
        return json!({ "ok": false, "msg": msg });
    }
    json!({ "ok": true, "msg": msg })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// 变量名大小写不敏感（Windows 规矩），lookup 两侧都归成大写再比
    fn fixed(map: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = map
            .iter()
            .map(|(k, v)| (k.to_ascii_uppercase(), v.to_string()))
            .collect();
        move |n: &str| m.get(&n.to_ascii_uppercase()).cloned()
    }

    fn none_lookup(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn expands_known_placeholders_and_keeps_unknown() {
        let l = fixed(&[("USERPROFILE", "C:\\Users\\X")]);
        assert_eq!(
            expand_vars("%USERPROFILE%\\Desktop", &l),
            "C:\\Users\\X\\Desktop"
        );
        // 变量名大小写不敏感是 Windows 的规矩
        assert_eq!(
            expand_vars("%userprofile%\\Desktop", &l),
            "C:\\Users\\X\\Desktop"
        );
        // 认不出的占位符原样留着——上层靠"仍是相对路径"发现并回退，绝不静默拼半截
        assert_eq!(expand_vars("%ONEDRIVE%\\Desktop", &l), "%ONEDRIVE%\\Desktop");
        // 中文路径、连续两个占位符、纯字面量都要过得了
        assert_eq!(
            expand_vars("%USERPROFILE%\\桌面\\%USERPROFILE%", &l),
            "C:\\Users\\X\\桌面\\C:\\Users\\X"
        );
        assert_eq!(expand_vars("D:\\OneDrive\\Desktop", &l), "D:\\OneDrive\\Desktop");
    }

    #[test]
    /// 英文别名那条必须落在中文名的同一个子目录里、只换文件名：
    /// 开始菜单按目录整理，同目录好认也好清；换目录会在 All Apps 里长出第二个分组
    #[test]
    fn search_alias_shares_the_chinese_folder() {
        let start = Path::new("C:\\Users\\X\\AppData\\Roaming\\Microsoft\\Windows\\Start Menu\\Programs\\Agent 驾驶舱\\Agent 驾驶舱.lnk");
        let alias = start_alias_of(start);
        assert_eq!(
            alias,
            PathBuf::from("C:\\Users\\X\\AppData\\Roaming\\Microsoft\\Windows\\Start Menu\\Programs\\Agent 驾驶舱\\AgentCockpit.lnk")
        );
        assert_eq!(alias.parent(), start.parent());
        assert_ne!(alias, start);
    }

    fn desktop_and_programs_resolve_with_registry_or_fallback() {
        let home = Path::new("C:\\Users\\X");
        let appdata = Path::new("C:\\Users\\X\\AppData\\Roaming");
        let none = none_lookup;

        // 注册表给的是未展开的 REG_EXPAND_SZ：要在 Rust 侧展开成绝对路径
        let (d, s) = resolve_lnk_paths(
            Some("%USERPROFILE%\\Desktop"),
            Some("%APPDATA%\\Microsoft\\Windows\\Start Menu\\Programs"),
            home,
            appdata,
            &none,
        );
        assert_eq!(d, PathBuf::from("C:\\Users\\X\\Desktop\\Agent 驾驶舱.lnk"));
        assert_eq!(
            s,
            PathBuf::from("C:\\Users\\X\\AppData\\Roaming\\Microsoft\\Windows\\Start Menu\\Programs\\Agent 驾驶舱\\Agent 驾驶舱.lnk")
        );

        // OneDrive 重定向桌面：注册表直接给绝对路径，必须照用而不是拼 %USERPROFILE%
        let (d2, _) = resolve_lnk_paths(
            Some("D:\\OneDrive\\Desktop"),
            None,
            home,
            appdata,
            &none,
        );
        assert_eq!(d2, PathBuf::from("D:\\OneDrive\\Desktop\\Agent 驾驶舱.lnk"));

        // 注册表缺失/为空 → 回退默认位置；认不出的占位符（展开后仍是相对路径）也回退
        let (d3, s3) = resolve_lnk_paths(Some(""), None, home, appdata, &none);
        assert_eq!(d3, PathBuf::from("C:\\Users\\X\\Desktop\\Agent 驾驶舱.lnk"));
        assert!(s3.starts_with(appdata));
        let (d4, _) = resolve_lnk_paths(Some("%NOPE%\\Desktop"), None, home, appdata, &none);
        assert_eq!(d4, PathBuf::from("C:\\Users\\X\\Desktop\\Agent 驾驶舱.lnk"));
    }

    #[test]
    fn only_exact_target_counts_as_ours() {
        let exe = Path::new("D:\\Claude\\bin\\AgentCockpit.exe");
        // 大小写、分隔符差异 Windows 视为同一路径，必须认账
        assert!(is_our_target("d:\\claude\\bin\\agentcockpit.EXE", exe));
        assert!(is_our_target("D:/Claude/bin/AgentCockpit.exe", exe));
        assert!(is_our_target("  \"D:\\Claude\\bin\\AgentCockpit.exe\" ", exe));
        // 同名不同目录、前缀相同的兄弟程序、空串一律拒绝——删除判据宁可错杀不可放行
        assert!(!is_our_target("C:\\other\\AgentCockpit.exe", exe));
        assert!(!is_our_target("D:\\Claude\\bin\\AgentCockpit.exe.bak", exe));
        assert!(!is_our_target("D:\\Claude\\bin\\AgentCockpit2.exe", exe));
        assert!(!is_our_target("", exe));
    }

    #[test]
    fn relative_or_placeholder_paths_are_rejected_as_abs() {
        assert!(is_abs_path("C:\\Users\\X\\Desktop"));
        assert!(is_abs_path("\\\\nas\\share\\x"));
        assert!(!is_abs_path("Desktop\\Agent.lnk"));
        assert!(!is_abs_path("%APPDATA%\\x"));
        assert!(!is_abs_path("C:x"));
        assert!(!is_abs_path(""));
    }

    #[test]
    fn ps_quote_doubles_inner_single_quotes() {
        assert_eq!(ps_quote("C:\\it's\\x"), "C:\\it''s\\x");
    }

    #[test]
    fn encoded_command_is_utf16le_base64() {
        // PowerShell 官方口径：脚本转 UTF-16LE 再标准 base64（"A" → "QQA="）
        assert_eq!(b64_utf16le(""), "");
        assert_eq!(b64_utf16le("A"), "QQA=");
        assert_eq!(b64_utf16le("Ab"), "QQBiAA==");
        // 中文名字若走这条编码不会在 GBK 命令行上变问号
        assert_eq!(b64_utf16le("舱"), "MYI=");
    }
}
