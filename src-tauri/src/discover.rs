// 自动发现本机已安装的 AI Agent：扫描开始菜单/桌面快捷方式，识别 Electron 隔离能力与档案目录
use crate::registry::{home, AppDef};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 命中这些关键词的程序算 AI 客户端/Agent
const AI_HINTS: &[&str] = &[
    "qoder", "workbuddy", "trae", "zcode", "codebuddy", "deepseek", "mimo", "opencode",
    "ollama", "chatgpt", "claude", "codex", "kimi", "moonshot", "marvis", "catpaw", "cockpit",
    "qwen", "tongyi", "通义", "千问", "doubao", "豆包", "yuanbao", "元宝", "hunyuan", "混元",
    "glm", "zhipu", "智谱", "minimax", "cursor", "windsurf", "copilot", "gemini", "dify",
    "anythingllm", "cherry studio", "cherrystudio", "openclaw", "iflow", "文心", "讯飞",
    "spark", "cline", "roo code", "aider", "continue", "zed", "suno", "langchain", "lm studio",
    "huggingface", "comfyui", "stable diffusion", "jan", "msty", "furina", "chatbox",
    "perplexity", "grok", "mistral", "cohere",
    "antigravity", "astudio", "ai studio", "aistudio",
];

/// 这些一律排除（许可证工具、卸载器、云盘/同步类不算 Agent）
const SKIP_HINTS: &[&str] = &[
    "uninstall", "uninst", "installer", "license", "deactivat", "activat", "reader",
    "documentacion", "solidnetwork", "license manager", "readme", "help", "support",
    "update", "updater", "agent-cockpit", "visual studio", "editorconfig",
    "云服务", "cloud", "drive", "netdisk", "网盘", "同步助手", "browser",
    "sogou", "搜狗", "皮肤盒子", "输入法", "microsoft input",
];

/// 用户自己的 AI 软件安装根目录：落在这里面的程序一律收录，新装即被发现
fn ai_install_roots() -> Vec<PathBuf> {
    vec![PathBuf::from("D:/Artificial_Intelligence")]
}

/// 路径归一化：统一分隔符与大小写，便于前缀比较，也便于和手工适配条目按**绝对路径**去重。
/// （Windows 传回的路径是反斜杠，如果直接和 "D:/Artificial_Intelligence" 比前缀永远不成立）
pub(crate) fn norm_path(p: &str) -> String {
    p.to_lowercase().replace('\\', "/")
}

pub(crate) fn is_ai(name: &str, exe: &str) -> bool {
    let hay = format!("{} {}", name, exe).to_lowercase();
    if SKIP_HINTS.iter().any(|s| hay.contains(s)) {
        return false;
    }
    if AI_HINTS.iter().any(|s| hay.contains(s)) {
        return true;
    }
    // 落在用户自己的 AI 安装根目录里的一律收录（分隔符/大小写无关）
    let exe_n = norm_path(exe);
    ai_install_roots()
        .iter()
        .any(|r| exe_n.starts_with(&norm_path(&r.to_string_lossy())))
}

/// Electron/Chromium 判定：同目录有 resources.pak / locales 才能用 --user-data-dir 隔离
pub(crate) fn is_chromium(exe: &Path) -> bool {
    let dir = match exe.parent() {
        Some(d) => d,
        None => return false,
    };
    let files = ["resources.pak", "icudtl.dat", "chrome_100_percent.pak"];
    let has_file = files
        .iter()
        .any(|f| dir.join(f).exists());
    let has_dir = dir.join("locales").is_dir();
    has_file && has_dir
}

pub(crate) fn slug(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_lowercase()
}

/// 归一化名：小写 + 去空格/连字符/下划线，用于与手工适配的条目去重
pub fn norm(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '_' | '\t'))
        .collect()
}

/// 找默认档案目录：只精确比对这几种常见位置，绝不做前缀模糊匹配
pub(crate) fn detect_profile(name: &str) -> Option<String> {
    let s = slug(name);
    // 显示名里的空格各家写法不同：WorkBuddy AI 的档案目录是 `.workbuddy-ai`，
    // 而 slug 会把空格直接吃掉变成 `workbuddyai` —— 两种写法都得试
    let mut slugs = vec![s.clone()];
    let hy = name.split_whitespace().collect::<Vec<_>>().join("-").to_lowercase();
    if hy != s {
        slugs.push(hy);
    }
    // 带后缀的显示名真实档案常常只取主名（"WorkBuddy AI" → "workbuddy"），放最后兜底
    if let Some(first) = name.split_whitespace().next().map(slug) {
        if !first.is_empty() && first != s {
            slugs.push(first);
        }
    }
    let mut cands: Vec<PathBuf> = Vec::new();
    for sl in &slugs {
        cands.push(home().join(format!(".{}", sl)));
    }
    if let Ok(ad) = std::env::var("APPDATA") {
        let ad = PathBuf::from(ad);
        cands.push(ad.join(name));
        for sl in &slugs {
            cands.push(ad.join(sl));
        }
    }
    if let Ok(la) = std::env::var("LOCALAPPDATA") {
        let la = PathBuf::from(la);
        cands.push(la.join(name));
        for sl in &slugs {
            cands.push(la.join(sl));
        }
    }
    // 别的程序已经认领的目录看都不看：抢错了，「沿用本机原有档案」就会把**别人**的
    // 真档案整个搬走 —— WorkBuddy AI 误认 `.workbuddy` 就这么把 287 个会话搬走过一次
    let claimed = crate::registry::claimed_profile_dirs();
    let key = |p: &PathBuf| {
        p.to_string_lossy()
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_lowercase()
    };
    let usable =
        |p: &PathBuf| p.is_dir() && p != home().as_path() && !claimed.contains(&key(p));
    // **有内容的优先**：空目录（比如只是被探测/启动顺手创建出来的 %APPDATA%\WorkBuddy AI）
    // 不该盖掉真正装着对话的那份，否则向导会说"目录是空的，没什么可接管"
    if let Some(p) = cands.iter().filter(|p| usable(p)).find(|p| crate::space::dir_has_data(p)) {
        return Some(p.to_string_lossy().into_owned());
    }
    cands
        .into_iter()
        .find(usable)
        .map(|p| p.to_string_lossy().into_owned())
}

fn scan_script() -> String {
    r#"[Console]::OutputEncoding = [Text.Encoding]::UTF8
$roots = @("$env:APPDATA\Microsoft\Windows\Start Menu\Programs", "$env:ProgramData\Microsoft\Windows\Start Menu\Programs", "$env:PUBLIC\Desktop", "$env:USERPROFILE\Desktop")
$sh = New-Object -ComObject WScript.Shell
$seen = New-Object 'System.Collections.Generic.HashSet[string]'
Get-ChildItem -Path $roots -Filter *.lnk -Recurse -ErrorAction SilentlyContinue | ForEach-Object {
  $t = $null
  try { $t = $sh.CreateShortcut($_.FullName).TargetPath } catch { }
  if (-not $t) { return }
  if ($t -notmatch '\.exe$') { return }
  if ($seen.Add($t.ToLower())) { "{0}|{1}" -f $_.BaseName, $t }
}"#
        .to_string()
}

/// 这条扫描候选是不是已被手工适配（内置/手动登记）认领——三把键任一命中即算：
/// ① 归一化显示名、② 归一化 exe 文件名、③ **归一化后的绝对路径**（大小写不敏感、
/// 分隔符统一）。名字键会失效的场景靠后两把兜底：内置条目"WorkBuddy 国际版"归一化成
/// `workbuddy国际版`，快捷方式"WorkBuddy AI"归一化成 `workbuddyai`——名字认不出，
/// 只有 exe 文件名键（WorkBuddyAI.exe）能认出它们是同一个程序。
/// 路径键则用于 exe 文件名太通用的兜底候选（千问办公的 Launcher.exe 不能按裸文件名
/// 全局认领，只能按完整路径认领它那一处）。
pub(crate) fn claimed_by_manual(skip: &HashSet<String>, name: &str, exe: &str) -> bool {
    let exe_file = Path::new(exe)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    skip.contains(&norm(name))
        || skip.contains(&norm(&exe_file))
        || skip.contains(&norm_path(exe))
}

/// 扫一遍开始菜单，返回去重后的 AI 程序（不含已手工适配的）
pub fn discover(skip_exe_names: &HashSet<String>) -> Vec<AppDef> {
    let mut c = std::process::Command::new("powershell");
    c.args(["-NoProfile", "-Command", &scan_script()]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    let out = match c.output() {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    // 脚本内已把控制台输出编码设为 UTF-8；异常字节按 lossy 兜底，不让扫描崩掉
    let text = String::from_utf8_lossy(&out.stdout).into_owned();

    let mut seen_targets: HashSet<String> = HashSet::new();
    let mut result: Vec<AppDef> = Vec::new();
    for line in text.lines() {
        let (name, exe) = match line.split_once('|') {
            Some(p) => p,
            None => continue,
        };
        let name = name.trim();
        let exe = exe.trim();
        if name.is_empty() || exe.is_empty() || !Path::new(exe).exists() {
            continue;
        }
        // 手工适配过的（名称/主 exe 文件名/解析后的绝对路径任一命中）不再重复收录
        if claimed_by_manual(skip_exe_names, name, exe) {
            continue;
        }
        if !is_ai(name, exe) {
            continue;
        }
        if !seen_targets.insert(exe.to_lowercase()) {
            continue;
        }
        let chromium = is_chromium(Path::new(exe));
        result.push(AppDef {
            id: format!("auto-{}", slug(name)),
            name: name.to_string(),
            kind: if chromium { "electron" } else { "plain" }.to_string(),
            exes: vec![exe.to_string()],
            env: String::new(),
            discovered: true,
            isolatable: chromium,
            profile: detect_profile(name),
            first_seen: 0,
            seen: false,
        });
    }
    result.sort_by(|a, b| a.name.cmp(&b.name));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手工适配的档案目录必须进"已被认领"清单——自动发现抢了它，
    /// 「沿用本机原有档案」就会把别人的真档案搬走（WorkBuddy AI 误认 .workbuddy 出过事）
    #[test]
    fn claimed_profiles_cover_curated_apps() {
        let c = crate::registry::claimed_profile_dirs();
        assert!(
            c.iter().any(|k| k.ends_with("/.workbuddy")),
            "WorkBuddy 的 .workbuddy 该被认领：{:?}",
            c
        );
        // Qoder 系的两个壳都该在清单里（候选用 | 分隔，两段都要展开）
        assert!(c.iter().any(|k| k.ends_with("com.qodercn.app.stable")));
        assert!(c.iter().any(|k| k.ends_with("/qoder") || k.ends_with("com.qoder.app.stable")));
        assert!(c.len() >= 8, "手工适配的档案目录不该这么少：{:?}", c);
        // 归一化：反斜杠、大小写、尾部分隔符都不能让同一个目录漏网
        assert!(c.iter().all(|k| !k.contains('\\') && *k == k.to_lowercase()));
    }

    /// Windows 实际传回的是反斜杠路径：安装根目录规则必须照样命中，
    /// 否则 D:\Artificial_Intelligence 下名字不含 AI 关键词的程序（AStudio 等）会被漏掉。
    #[test]
    fn install_root_matches_windows_backslash_paths() {
        assert!(is_ai("AStudio", r"D:\Artificial_Intelligence\AStudio\AStudio.exe"));
        assert!(is_ai("SomeTool", r"D:\Artificial_Intelligence\SomeTool\SomeTool.exe"));
        assert!(is_ai("AStudio", "D:/Artificial_Intelligence/AStudio/AStudio.exe"));
    }

    /// 装在别处、但名字本身是 AI 产品的也要能认出来（Antigravity 装在 AppData 下）
    #[test]
    fn ai_products_outside_root_are_found_by_name() {
        assert!(is_ai(
            "Antigravity",
            r"C:\Users\x\AppData\Local\Programs\antigravity\Antigravity.exe"
        ));
    }

    #[test]
    fn uninstallers_and_plain_apps_stay_out() {
        assert!(!is_ai("Uninstall AStudio", r"D:\Artificial_Intelligence\AStudio\Uninstall AStudio.exe"));
        assert!(!is_ai("Notepad", r"C:\Windows\notepad.exe"));
    }

    /// 真机才跑：真的扫一遍开始菜单，确认 AStudio / Antigravity 这类
    /// 「名字不含 AI 关键词但装在安装根目录」的程序能被收录。
    /// cargo test -- --include-ignored
    #[test]
    #[ignore]
    fn real_scan_finds_astudio_and_antigravity() {
        let found = discover(&HashSet::new());
        let names: Vec<String> = found.iter().map(|a| a.name.to_lowercase()).collect();
        println!("本机扫到 {} 个：{:?}", names.len(), names);
        assert!(names.iter().any(|n| n.contains("astudio")), "漏了 AStudio：{names:?}");
        assert!(names.iter().any(|n| n.contains("antigravity")), "漏了 Antigravity：{names:?}");
    }

    /// 钉死 WorkBuddy 国际版那条 bug：内置 `workbuddy-intl`（exe 已改指 WorkBuddyAI.exe）
    /// 存在时，发现"WorkBuddy AI / WorkBuddyAI.exe"这条快捷方式**不得**再生成
    /// auto-workbuddyai 和用户看到的双卡。名字键在这里是认不出来的
    /// （`workbuddy国际版` ≠ `workbuddyai`），救场的是 exe 文件名键——
    /// 所以断言分两半：先证明名字键确实失灵，再证明整判据仍然拦住。
    #[test]
    fn manual_workbuddy_intl_claims_workbuddyai_by_exe_key() {
        let skip = crate::registry::manual_claimed_names();
        // ① 名字键失灵（这正是当初 auto-workbuddyai 冒出来的原因）
        assert!(
            !skip.contains(&norm("WorkBuddy AI")),
            "内置显示名是「WorkBuddy 国际版」，归一化名字键本就不该等于 workbuddyai"
        );
        // ② exe 文件名键必须参与去重：WorkBuddyAI.exe 已被内置条目认领
        assert!(skip.contains(&norm("WorkBuddyAI.exe")), "exe 键没进认领集合：去重会漏");
        // ③ 完整判据拦住扫描候选（反斜杠路径、大小写都得认）
        assert!(
            claimed_by_manual(&skip, "WorkBuddy AI", r"D:\Artificial_Intelligence\WorkBuddyAI\WorkBuddyAI.exe"),
            "内置 workbuddy-intl 存在时不得再生成 auto-workbuddyai"
        );
        assert!(
            claimed_by_manual(&skip, "workbuddy ai", r"d:\ARTIFICIAL_INTELLIGENCE\workbuddyai\WORKBUDDYAI.EXE"),
            "比对是大小写不敏感的绝对路径/归一化名，换个写法也要拦住"
        );
        // ④ 国内版那条不能被国际版 exe 顶掉（两张卡是两个安装，各自认各自的）
        assert!(skip.contains(&norm("WorkBuddy.exe")));
        assert!(skip.contains(&norm("WorkBuddyAI.exe")));
    }

    #[test]
    fn curated_claude_desktop_claims_its_shortcut_without_guessing_a_profile() {
        let skip = crate::registry::manual_claimed_names();
        assert!(claimed_by_manual(
            &skip,
            "Claude",
            r"C:\Program Files\WindowsApps\Claude_3.6556.2.0_x64__pzs8sxrjxfjjc\app\Claude.exe"
        ), "Claude 快捷方式不得再生成可隔离的自动副本");
        assert!(skip.contains(&norm("claude.cmd")), "CLI 的入口仍独立登记");
        assert!(!claimed_by_manual(&skip, "Claude helper", r"C:\Tools\Claude-helper.exe"));
    }

    /// 千问办公：内置条目主 exe 是版本目录里的 QwenWorkCN.exe，兜底候选才是启动壳
    /// Launcher.exe。快捷方式指的就是 Launcher.exe——裸文件名 "launcher.exe" 不能
    /// 全局认领（会误吞别家恰好叫 Launcher.exe 的程序），拦住它的是解析后的绝对路径键。
    #[test]
    fn qwenwork_launcher_claimed_by_full_path_not_bare_name() {
        let skip = crate::registry::manual_claimed_names();
        // 显示名键也命中（快捷方式名就叫「千问办公」），路径键兜住别的写法
        assert!(claimed_by_manual(
            &skip,
            "千问办公",
            r"D:\Artificial_Intelligence\QwenWorkCN\Launcher.exe"
        ));
        assert!(claimed_by_manual(
            &skip,
            "QwenWork 启动器",
            "d:/artificial_intelligence/qwenworkcn/launcher.exe"
        ));
        // 别的路径上的 Launcher.exe 不许被误吞
        assert!(
            !skip.contains(&norm("Launcher.exe")),
            "裸文件名 launcher.exe 不该被全局认领"
        );
        assert!(!claimed_by_manual(&skip, "Foo AI", r"C:\Games\Foo\Launcher.exe"));
    }
}
