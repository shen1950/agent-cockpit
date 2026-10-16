// Agent 注册表 + 本地存储（实例档案 / 配置）
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub const PROXY_PORT: u16 = 8788;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct AppDef {
    pub id: String,
    pub name: String,
    pub kind: String, // electron | cli | plain
    pub exes: Vec<String>, // 支持 {LOCALAPPDATA} 占位符，按顺序取第一个存在的
    pub env: String,       // CLI 系的隔离环境变量
    pub discovered: bool,  // 自动发现（非手工适配）
    pub isolatable: bool,  // 能否用 --user-data-dir / 环境变量隔离多开
    pub profile: Option<String>, // 默认档案目录（换号模式用）
    #[serde(default)]
    pub first_seen: u64,   // 自动发现首次收录的 unix 秒；内置/手动登记为 0
    #[serde(default)]
    pub seen: bool,        // 用户点开看过一次 → 不再标「新」
}

fn curated(
    id: &str, name: &str, kind: &str, exes: &[&str], env: &str, profile: Option<&str>,
    isolatable: bool,
) -> AppDef {
    AppDef {
        id: id.into(),
        name: name.into(),
        kind: kind.into(),
        exes: exes.iter().map(|s| s.to_string()).collect(),
        env: env.into(),
        discovered: false,
        isolatable,
        profile: profile.map(|s| s.to_string()),
        first_seen: 0,
        seen: true,   // 内置/手工登记的从来不需要标「新」
    }
}

/// isolatable 的取值来自本机实测：第二次启动若被已有窗口接管（参数被丢弃）就是 false
pub fn apps() -> Vec<AppDef> {
    vec![
        // Qoder 的「档案」必须是登录态真正待着的那个目录。
        // 2026-10-08 实测：0.4.x 换了壳，登录令牌写成了
        //   %APPDATA%\com.<bundle>.app.stable\auth.v1.dat（v10 加密的 {token,expiresAt,user}），
        // 旧的 VS Code 壳那套 %APPDATA%\Qoder\User\globalStorage\state.vscdb 自 8-27 起就没人写了。
        // 早先更错的一版写的是 ~/.qoder —— 那是 Qoder Agent 的运行时家目录（.auth/.bin/ipc/
        // host-actions/mcp-router.json），两个后果都是坏的：
        //   ① 换号换不动登录（IDE 读的不是它），打开还是原来那个号；
        //   ② ~/.qoder 只要有一个 agent 会话在跑就被占着，rename 直接「拒绝访问」。
        // 所以：候选里放上两个壳的目录，由 resolve_profile 挑真有登录数据的那个。
        curated("qoder", "Qoder", "electron",
            &["{LOCALAPPDATA}/Programs/Qoder/Qoder.exe", "D:/Artificial_Intelligence/Qoder/Qoder.exe"],
            "", Some("{APPDATA}/com.qoder.app.stable|{APPDATA}/Qoder"), false),
        curated("qoder-cn", "Qoder CN", "electron",
            &["D:/Artificial_Intelligence/Qoder CN/Qoder CN.exe"], "",
            Some("{APPDATA}/com.qodercn.app.stable|{APPDATA}/QoderCN"), false),
        curated("qoder-cn-ide", "Qoder CN IDE", "electron",
            &["D:/Artificial_Intelligence/Qoder CN IDE/Qoder CN IDE.exe"], "", Some(".qoder-cn-ide"), true),
        curated("workbuddy", "WorkBuddy", "electron",
            &["D:/LenovoSoftstore/Install/WorkBuddy/WorkBuddy.exe"], "", Some(".workbuddy"), false),
        // WorkBuddy 国际版是**独立的一份安装**，不是国内版的另一张卡。2026-10-11 磁盘证据：
        //   国内版  D:\LenovoSoftstore\Install\WorkBuddy\WorkBuddy.exe（FileVersion 5.7.7，ProductName "WorkBuddy"），
        //           站点 www.workbuddy.cn，档案 ~\.workbuddy（账号 DuckDeity，用户目录 06708772-…）
        //   国际版  D:\Artificial_Intelligence\WorkBuddyAI\WorkBuddyAI.exe（FileVersion 5.7.6，ProductName "WorkBuddy AI"），
        //           站点 www.workbuddy.ai，档案 ~\.workbuddy-ai（账号 shen1950，用户目录 403fabba-…）
        // 两份安装各自有 204,585,0xx 字节的 exe，只差那一个 "AI" 后缀——是两个安装、两套档案，程序同源。
        // 登录态也各存各的：%LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\ 下
        //   国内版 workbuddy-desktop.info / 国际版 workbuddy-desktop-ai.info。
        // 早先这条 curated 的 exe 和 profile 全抄了国内版（同一份档案的两张卡），
        // 真正的国际版安装被自动发现收成第三条 auto-workbuddyai，用户因此看到
        // 「WorkBuddy 国际版」和「WorkBuddy AI」并存——那是登记错了，不是重复收录。
        // 现在 exe/profile/lockers 三处都必须只认国际版自己那套（见 lockers() 的 workbuddy-intl）。
        curated("workbuddy-intl", "WorkBuddy 国际版", "electron",
            &["D:/Artificial_Intelligence/WorkBuddyAI/WorkBuddyAI.exe"], "", Some(".workbuddy-ai"), false),
        // AStudio（科大讯飞 星辰/星火）：登录态明文落在安装目录**同级**的
        //   <安装目录上一级>/AStudio Data/userdata/astron-session.json
        // （asar 里 resolveWindowsDataLocationRoot：parent(dirname(installDir)) + "AStudio Data"）。
        // 单实例产品，只能整目录换号。
        curated("astudio", "AStudio", "electron",
            &["D:/Artificial_Intelligence/AStudio/AStudio.exe"], "",
            Some("D:/Artificial_Intelligence/AStudio Data/userdata"), false),
        // MiniMax Code：OAuth 令牌落 ~/.minimax/auth/<env>/<region>/mcode-public/。
        // 家目录就是 ~/.minimax，单实例产品，整目录换号。
        curated("minimax", "MiniMax Code", "electron",
            &["D:/Artificial_Intelligence/MiniMax Code/MiniMax Code.exe"], "", Some(".minimax"), false),
        // TRAE CN（原 TraeWork）：2026-10-11 官方升级改了名，安装目录
        // TRAE SOLO CN → Trae CN，登录档案也跟着搬（%APPDATA%\Trae CN，migration-job.json
        // 记录了迁移；加密格式没变，仍是 byteCrypto）。exe 与档案候选都留两段：
        // 新版优先，还没升级的机器回落旧目录，resolve_profile 挑真有登录数据的那个。
        // ~/.trae-cn 依旧是运行时家目录（assistant/ 对话历史、extensions/ plugins/ skills/
        // mcps/ toolhost/），跟登录态无关——换它既换不动登录，又会把正在用的插件搬乱。
        curated("traework-cn", "TRAE CN", "electron",
            &["D:/Artificial_Intelligence/Trae CN/Trae CN.exe",
              "D:/Artificial_Intelligence/TRAE SOLO CN/TRAE SOLO CN.exe"], "",
            Some("{APPDATA}/Trae CN|{APPDATA}/TRAE SOLO CN"), true),
        curated("zcode", "ZCode", "electron",
            &["D:/Artificial_Intelligence/ZCode/ZCode.exe"], "", Some(".zcode"), true),
        // 千问办公（QwenWorkCN）：根目录那个 Launcher.exe 只是启动壳（同目录没有
        // resources.pak/locales，不是 Electron 主程序）；真正的 Electron 主程序在版本目录
        //   D:\Artificial_Intelligence\QwenWorkCN\<版本>\QwenWorkCN.exe
        // （updater.cfg 的 currentVersion 指着哪个版本；exe 文件名不随版本变）。
        // 候选顺序：版本目录在前（拿到的是真主程序，进程名匹配/--user-data-dir 都按它算），
        // Launcher.exe 兜底（官方升级后版本目录改名，这里还能起得来，dedup 也靠
        // manual_claimed_names 认领全部候选路径，不会漏出一条 auto-千问办公）。
        // 档案 {APPDATA}\QwenWorkCN（auth-v2.dat 是 Chromium os_crypt v10 信封；
        // 同目录 auth.dat 是 v1 旧壳，里面 token 是 "placeholder-" 占位串，别读它）。
        // isolatable=false：未实测 --user-data-dir 隔离性，先按不可多开处理（猜错的后果是
        // "新建了账号2，点启动唤出的还是同一个账号"）。
        curated("qwenwork", "千问办公", "electron",
            &["D:/Artificial_Intelligence/QwenWorkCN/1.2.5-26092902/QwenWorkCN.exe",
              "D:/Artificial_Intelligence/QwenWorkCN/Launcher.exe"], "",
            Some("{APPDATA}/QwenWorkCN|{APPDATA}/QwenWork|{LOCALAPPDATA}/QwenWorkCN"), false),
        // DeepSeek Harness：Electron（同目录有 resources.pak+locales，实测）。
        // 登录态是明文 ~/.dsh/.credentials.yaml（grant deepseek-account-platform/default 的 token），
        // Electron userData 在 {APPDATA}\@deepseek-ai\dsh-desktop——换号两处都要算，候选写法照 Qoder 系。
        // resolve_profile 靠 has_login_data 认 .credentials.yaml 落在 ~/.dsh。
        // isolatable=false：未实测 --user-data-dir 隔离性，先按不可多开处理。
        curated("dsh", "DeepSeek Harness", "electron",
            &["D:/Artificial_Intelligence/DeepSeek Harness/DeepSeek Harness.exe"], "",
            Some(".dsh|{APPDATA}/@deepseek-ai/dsh-desktop"), false),
        // Marvis（腾讯）：Qt+CEF 壳，不是 Electron（安装目录没有 resources.pak/locales），按 plain 登记。
        // 档案 ~\.marvis **本身是个 symlink** → %APPDATA%\Tencent\Marvis\User\<openid>\，
        // openid 编在链接目标里：产品自己换账号会重指这条链接，驾驶舱的 junction 换号
        // 再往上挂就是抢它自己的链接管理——所以 marvis 的登录态只能当"整机一份"看待
        // （直读永远回落，见 local_quota::falls_back_to_default），按账号目录挂链接这条路
        // 目前不成立。登录态 .marvis\mobile_address.json（明文 access_token/mobile_open_id）。
        // isolatable=false：未实测（而且 plain 壳本来就走不了 --user-data-dir）。
        curated("marvis", "Marvis", "plain",
            &["D:/Artificial_Intelligence/Marvis/Application/MarvisLauncher.exe"], "", Some(".marvis"), false),
        // Xiaomi MiMo：Electron（实测同目录有 resources.pak+locales）。
        // 档案 {APPDATA}\Xiaomi MiMo，登录态在 Partitions\xiaomi-account\Network\Cookies
        // （SQLite，os_crypt 加密；userId=2802645431）——多分区 Electron 应用，
        // 整机一份，不在账号实例目录（直读永远回落）。
        // isolatable=false：未实测多分区应用吃 --user-data-dir 的行为，先按不可多开处理。
        curated("xiaomi-mimo", "Xiaomi MiMo", "electron",
            &["D:/Artificial_Intelligence/Xiaomi MiMo/Xiaomi MiMo.exe"], "",
            Some("{APPDATA}/Xiaomi MiMo"), false),
        // OpenCode：只登记档案目录，不接直读（这台机器它从未登录过官方账号——
        // %USERPROFILE%\.local\share\opencode\opencode.db 的 account 三表 0 行；
        // 而且产品自身没有任何余额接口，所以 LOCAL_STORES 里没有它）。
        // 登记 profile 是为了换号/多开有落点：数据家目录 ~/.local/share/opencode，
        // Electron userData {APPDATA}\ai.opencode.desktop（两处都实测存在）。
        // isolatable=false：自动发现曾按 Chromium 特征给过 true，但没实测第二次启动是否
        // 被已有窗口接管，按规矩收回 false。
        curated("opencode", "OpenCode", "electron",
            &["{LOCALAPPDATA}/Programs/@opencode-aidesktop/OpenCode.exe"], "",
            Some(".local/share/opencode|{APPDATA}/ai.opencode.desktop"), false),
        curated("codex", "Codex CLI", "cli", &["D:/npm-global/codex.cmd"], "CODEX_HOME", Some(".codex"), true),
        curated("claude", "Claude Code CLI", "cli", &["D:/npm-global/claude.cmd"], "CLAUDE_CONFIG_DIR", Some(".claude"), true),
        // ------------------------------------------------------------------
        // 以下这批是 2026-10-16 照着 jlcodes99/cockpit-tools 的平台清单补的：
        // Antigravity / Cursor / Kiro / Windsurf / GitHub Copilot / Zed / Grok Bot /
        // CodeBuddy(CN) / Trae(国际) / TRAE SOLO(CN)。
        // 路径一律先给**官方默认安装位置**（%LOCALAPPDATA%\Programs\<名> 或 %ProgramFiles%），
        // 再把本机 D:\Artificial_Intelligence\ 下的实际位置作为候选兜底——
        // 这份登记是要跟着仓库发出去的，别人机器上没有 D 盘那一层，默认位置必须排第一。
        // kind 一律 electron：这几个（除 Grok Bot 待定）都是 VS Code / Chromium 壳，
        // isolatable 先按"能吃 --user-data-dir"记 true，与本机实测过的 Trae CN / ZCode 同规格。
        curated("antigravity", "Antigravity", "electron",
            &["{LOCALAPPDATA}/Programs/antigravity/Antigravity.exe",
              "{LOCALAPPDATA}/Programs/Antigravity/Antigravity.exe",
              "{PROGRAMFILES}/Antigravity/Antigravity.exe"], "",
            Some("{APPDATA}/Antigravity"), true),
        curated("cursor", "Cursor", "electron",
            &["{LOCALAPPDATA}/Programs/cursor/Cursor.exe",
              "{LOCALAPPDATA}/Programs/Cursor/Cursor.exe",
              "{PROGRAMFILES}/cursor/Cursor.exe"], "",
            Some("{APPDATA}/Cursor"), true),
        curated("kiro", "Kiro", "electron",
            &["{LOCALAPPDATA}/Programs/Kiro/Kiro.exe",
              "{PROGRAMFILES}/Kiro/Kiro.exe"], "",
            Some("{APPDATA}/Kiro"), true),
        curated("windsurf", "Windsurf", "electron",
            &["{LOCALAPPDATA}/Programs/Windsurf/Windsurf.exe",
              "{PROGRAMFILES}/Windsurf/Windsurf.exe"], "",
            Some("{APPDATA}/Windsurf"), true),
        // GitHub Copilot 本身不是独立程序，它跑在 VS Code 里——档案就是 VS Code 的 userData。
        // 登录态在 User/globalStorage/state.vscdb 里那块 github.copilot 的 secret。
        curated("github-copilot", "GitHub Copilot", "electron",
            &["{LOCALAPPDATA}/Programs/Microsoft VS Code/Code.exe",
              "{PROGRAMFILES}/Microsoft VS Code/Code.exe"], "",
            Some("{APPDATA}/Code|{APPDATA}/Code - Insiders"), true),
        curated("zed", "Zed", "electron",
            &["{LOCALAPPDATA}/Programs/Zed/Zed.exe", "{PROGRAMFILES}/Zed/Zed.exe"], "",
            Some("{APPDATA}/Zed"), true),
        // Grok：xAI 的桌面客户端叫 Grok Bot（Electron 壳，同目录有 resources.pak+locales）。
        // CLI 那份 ~/.grok 是它自己的运行时家目录，一并放进候选，换号两处都算得到。
        curated("grok", "Grok Bot", "electron",
            &["{LOCALAPPDATA}/Programs/Grok Bot/Grok Bot.exe",
              "D:/Artificial_Intelligence/Grok Bot/Grok Bot.exe"], "",
            Some("{APPDATA}/Grok Bot|.grok"), true),
        curated("codebuddy", "CodeBuddy", "electron",
            &["{LOCALAPPDATA}/Programs/CodeBuddy/CodeBuddy.exe",
              "{PROGRAMFILES}/CodeBuddy/CodeBuddy.exe"], "",
            Some("{APPDATA}/CodeBuddy"), true),
        curated("codebuddy-cn", "CodeBuddy CN", "electron",
            &["{LOCALAPPDATA}/Programs/CodeBuddy CN/CodeBuddy CN.exe",
              "{PROGRAMFILES}/CodeBuddy CN/CodeBuddy CN.exe"], "",
            Some("{APPDATA}/CodeBuddy CN"), true),
        curated("trae", "Trae", "electron",
            &["{LOCALAPPDATA}/Programs/Trae/Trae.exe", "{PROGRAMFILES}/Trae/Trae.exe"], "",
            Some("{APPDATA}/Trae"), true),
        curated("trae-solo", "TRAE SOLO", "electron",
            &["{LOCALAPPDATA}/Programs/TRAE SOLO/TRAE SOLO.exe",
              "{PROGRAMFILES}/TRAE SOLO/TRAE SOLO.exe"], "",
            Some("{APPDATA}/TRAE SOLO"), true),
        curated("trae-solo-cn", "TRAE SOLO CN", "electron",
            &["D:/Artificial_Intelligence/TRAE SOLO CN/TRAE SOLO CN.exe",
              "{LOCALAPPDATA}/Programs/TRAE SOLO CN/TRAE SOLO CN.exe"], "",
            Some("{APPDATA}/TRAE SOLO CN"), true),
    ]
}

pub fn app_def(app_id: &str) -> Option<AppDef> {
    // 用 raw：被隐藏的条目仍要能被操作（否则无法"恢复显示"）
    all_apps_raw().into_iter().find(|a| a.id == app_id)
}

fn discovered_cache() -> &'static std::sync::Mutex<Vec<AppDef>> {
    static C: std::sync::OnceLock<std::sync::Mutex<Vec<AppDef>>> = std::sync::OnceLock::new();
    C.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 自动发现的历史快照：存进 config，保证下次启动/重扫仍能识别「已卸载」的程序
pub fn discovered_seen() -> Vec<AppDef> {
    load_config()
        .get("discovered_apps")
        .and_then(|v| serde_json::from_value::<Vec<AppDef>>(v.clone()).ok())
        .unwrap_or_default()
}

/// 手工适配（内置 + 手动登记）已经认领的名字与 exe 名（归一化）。
/// 自动扫描跳过这些，历史快照里撞上的条目也该摘掉——不然产品改名升级
/// （TraeWork→TRAE CN，2026-10-11）后，同一个 exe 会留下一条永远不再更新的
/// 自动条目，跟内置条目抢同一个程序。
/// 判据是**双键**：① 归一化显示名 + 归一化主 exe 文件名；② 全部 exe 候选解析后的
/// **绝对路径**（大小写、分隔符归一）。第二把键是给"名字键认不出来"的场景兜底的：
/// WorkBuddy 国际版（内置显示名归一化 `workbuddy国际版`）对 WorkBuddy AI（快捷方式名
/// 归一化 `workbuddyai`）靠的是 exe 键 `workbuddyai.exe`；千问办公的启动壳 Launcher.exe
/// 文件名太通用、不能按裸文件名全局认领（会误吞别家恰好叫 Launcher.exe 的程序），
/// 但它落在候选里——按解析后的绝对路径认领，既摘掉快照里的 auto-千问办公，
/// 又不动别的路径上的同名壳。
pub fn manual_claimed_names() -> std::collections::HashSet<String> {
    let mut skip: std::collections::HashSet<String> = std::collections::HashSet::new();
    for a in apps().into_iter().chain(custom_apps()) {
        skip.insert(crate::discover::norm(&a.name));
        // 主 exe 文件名认领（exes[0] 是产品真身；兜底候选只认领完整路径，见上）
        skip.insert(crate::discover::norm(&exe_name(&a)));
        for x in &a.exes {
            skip.insert(crate::discover::norm_path(&expand(x).to_string_lossy()));
        }
    }
    skip.insert(crate::discover::norm("agent-cockpit"));
    skip
}

fn save_discovered_seen(list: &[AppDef]) {
    let mut cfg = load_config();
    cfg["discovered_apps"] = serde_json::to_value(list).unwrap_or_else(|_| json!([]));
    save_config(&cfg);
}

/// 把本次扫描结果与历史快照合并：
/// - 新出现的程序记 first_seen（用于侧栏「新」标记）
/// - 历史收录过、这次没扫到且 exe 已消失的，保留下来（识别为「已卸载」）
/// - 历史收录过、这次没扫到但 exe 还在的（如快捷方式被删），照旧保留为已安装
pub fn set_discovered(fresh: Vec<AppDef>) {
    let now = now_secs();
    // 手工适配已认领的 exe（主文件名或解析后的绝对路径）：历史快照里撞上的条目不再保留。
    // 这种条目本来就被 all_apps_raw 的同名去重遮蔽（界面上看不见），留着只会在 config 里堆僵尸——
    // 产品改名升级（TraeWork→TRAE CN，2026-10-11）后，同一个 exe 会先后记成两个 id；
    // 修正内置条目的 exe 之后（workbuddy-intl 改指 WorkBuddyAI.exe，2026-10-11），
    // 原先被错误登记遮蔽的 auto-workbuddyai 也得跟着摘掉，不然它又浮出来和内置条目并列。
    // 主文件名按 norm 比、绝对路径按归一化（大小写/分隔符）比；不按显示名，
    // 同名不同 exe 的程序（便携版等）是两个东西，别误删。
    let claimed = manual_claimed_names();
    let claimed_entry = |a: &AppDef| -> bool {
        if claimed.contains(&crate::discover::norm(&exe_name(a))) {
            return true;
        }
        match a.exes.first() {
            Some(x) => claimed.contains(&crate::discover::norm_path(x)),
            None => false,
        }
    };
    let mut prev: std::collections::HashMap<String, AppDef> = discovered_seen()
        .into_iter()
        .filter(|a| !claimed_entry(a))
        .map(|a| {
            let key = a.exes.first().cloned().unwrap_or_default().to_lowercase();
            (key, a)
        })
        .collect();
    let mut out: Vec<AppDef> = Vec::new();
    for mut a in fresh {
        let key = a.exes.first().cloned().unwrap_or_default().to_lowercase();
        match prev.remove(&key) {
            Some(p) => {
                a.first_seen = p.first_seen.max(1);
                a.seen = p.seen;   // 用户已经点开看过，下次扫描也别再标「新」
            }
            None => {
                if a.first_seen == 0 {
                    a.first_seen = now;
                }
            }
        }
        out.push(a);
    }
    out.extend(prev.into_values());
    out.sort_by(|a, b| a.name.cmp(&b.name));
    save_discovered_seen(&out);
    let mut g = match discovered_cache().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    *g = out;
}

pub fn discovered() -> Vec<AppDef> {
    let cached = match discovered_cache().lock() {
        Ok(g) => g.clone(),
        Err(p) => p.into_inner().clone(),
    };
    if !cached.is_empty() {
        return cached;
    }
    // 本次会话还没扫过：用持久化快照顶上，保证启动瞬间侧栏就有已发现的程序
    let seen = discovered_seen();
    let mut g = match discovered_cache().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    *g = seen.clone();
    seen
}

/// 「清理记录」：把某条自动发现的程序从列表与快照里摘掉（不碰账号目录）
pub fn forget_discovered(app_id: &str) -> bool {
    let mut seen = discovered_seen();
    let before = seen.len();
    seen.retain(|a| a.id != app_id);
    if seen.len() == before {
        return false;
    }
    save_discovered_seen(&seen);
    let mut g = match discovered_cache().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    g.retain(|a| a.id != app_id);
    true
}

/// 用户点开看过某条自动收录的程序：记下来，侧栏不再标「新」。
/// 只在第一次点开时写盘，避免每次点击都动 config.json。
pub fn mark_seen(app_id: &str) -> bool {
    let mut list = discovered_seen();
    let mut hit = false;
    for a in list.iter_mut() {
        if a.id == app_id && !a.seen {
            a.seen = true;
            hit = true;
        }
    }
    if hit {
        save_discovered_seen(&list);
        let mut g = match discovered_cache().lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        for a in g.iter_mut() {
            if a.id == app_id {
                a.seen = true;
            }
        }
    }
    hit
}

/// 用户手动登记的程序：自动扫描认不出的那些（绿色版、没建快捷方式、名字不含 AI 关键词）
pub fn custom_apps() -> Vec<AppDef> {
    load_config()
        .get("custom_apps")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|c| {
            let exe = c.get("exe")?.as_str()?;
            let name = c.get("name")?.as_str()?;
            if exe.is_empty() || name.is_empty() {
                return None;
            }
            Some(AppDef {
                id: custom_id(name),
                name: name.to_string(),
                kind: c.get("kind").and_then(|v| v.as_str()).unwrap_or("plain").to_string(),
                exes: vec![exe.to_string()],
                env: String::new(),
                discovered: false,
                isolatable: c.get("isolatable").and_then(|v| v.as_bool()).unwrap_or(false),
                profile: c.get("profile").and_then(|v| v.as_str()).map(String::from),
                first_seen: 0,
                seen: true,   // 手工登记的从不标「新」
            })
        })
        .collect()
}

pub fn custom_id(name: &str) -> String {
    format!("custom-{}", crate::discover::slug(name))
}

/// 按可执行文件同目录的特征判断能不能用 --user-data-dir 多开
pub fn make_custom(name: &str, exe: &str) -> AppDef {
    let chromium = crate::discover::is_chromium(std::path::Path::new(exe));
    AppDef {
        id: custom_id(name),
        name: name.to_string(),
        kind: if chromium { "electron" } else { "plain" }.to_string(),
        exes: vec![exe.to_string()],
        env: String::new(),
        discovered: false,
        isolatable: chromium,
        profile: crate::discover::detect_profile(name),
        first_seen: 0,
        seen: true,
    }
}

pub fn save_custom(def: &AppDef) {
    let mut cfg = load_config();
    let mut list = cfg
        .get("custom_apps")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let exe = def.exes.first().cloned().unwrap_or_default();
    list.retain(|c| {
        c.get("id").and_then(|v| v.as_str()) != Some(&def.id)
            && c.get("exe").and_then(|v| v.as_str()) != Some(&exe)
    });
    list.push(json!({
        "id": def.id, "name": def.name, "exe": exe, "kind": def.kind,
        "isolatable": def.isolatable, "profile": def.profile,
    }));
    cfg["custom_apps"] = json!(list);
    save_config(&cfg);
}

/// 只从列表里摘掉，账号目录与实例档案一律不动
pub fn remove_custom(app_id: &str) -> bool {
    let mut cfg = load_config();
    let mut list = cfg
        .get("custom_apps")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let before = list.len();
    list.retain(|c| c.get("id").and_then(|v| v.as_str()) != Some(app_id));
    if list.len() == before {
        return false;
    }
    cfg["custom_apps"] = json!(list);
    save_config(&cfg);
    true
}

/// 手工适配的 + 手动添加的 + 自动发现的；同一可执行文件只留靠前的那份。
/// 认领键与 manual_claimed_names 同一套判据：exe 文件名 + **全部候选解析后的绝对路径**
/// （大小写/分隔符归一）。只按文件名会漏：千问办公内置条目主 exe 是版本目录里的
/// QwenWorkCN.exe，自动发现那条记的是启动壳 Launcher.exe——名字键对不上，
/// 靠 Launcher.exe 的完整路径键才能把它遮蔽掉。
pub fn all_apps_raw() -> Vec<AppDef> {
    let mut out = apps();
    let mut taken: std::collections::HashSet<String> = out
        .iter()
        .flat_map(|a| {
            let n = a.exes.iter().map(|x| crate::discover::norm_path(&expand(x).to_string_lossy()));
            std::iter::once(exe_name(a).to_lowercase()).chain(n).collect::<Vec<_>>()
        })
        .collect();
    for d in custom_apps().into_iter().chain(discovered()) {
        let path_hit = d
            .exes
            .first()
            .map(|x| taken.contains(&crate::discover::norm_path(x)))
            .unwrap_or(false);
        if !path_hit && taken.insert(exe_name(&d).to_lowercase()) {
            out.push(d);
        }
    }
    out
}

/// 所有已知程序（手工适配 + 手动添加）认领的档案目录（归一化键）。
/// 自动发现猜目录时必须避开：抢了**别人**的默认目录，之后「沿用本机原有档案」
/// 会把别人的真档案整个搬走 —— WorkBuddy AI 误认 `.workbuddy` 就这么出过一次事。
pub fn claimed_profile_dirs() -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for a in apps().into_iter().chain(custom_apps()) {
        if let Some(spec) = a.profile.as_deref() {
            for one in spec.split('|').map(str::trim).filter(|s| !s.is_empty()) {
                out.insert(path_key(&one_profile(one).to_string_lossy()));
            }
        }
    }
    out
}

/// 左侧列表的手动次序（把常用的挪到前面）。没列进去的程序按原顺序跟在后面。
pub fn app_order() -> Vec<String> {
    crate::settings::get()
        .get("app_order")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

pub fn set_app_order(ids: &[String]) {
    let mut cfg = load_config();
    cfg["settings"]["app_order"] = json!(ids);
    save_config(&cfg);
}

/// 按用户排的次序稳定排序：排过的排前面并保持他们之间的顺序，没排过的原样跟在后面
pub fn sort_apps(out: &mut Vec<AppDef>) {
    let order = app_order();
    if order.is_empty() {
        return;
    }
    out.sort_by_key(|a| order.iter().position(|x| *x == a.id).unwrap_or(order.len()));
}

/// 各家 Agent 的限时/每日福利活动（"Qoder 每天免费用 Qwen3.8-Max"这类）。
/// 这类信息官方没有接口，全靠手动记一条，所以存一个独立小文件，坏了也不影响账号档案。
pub fn perks_file() -> PathBuf {
    base_dir().join("perks.json")
}

pub fn load_perks() -> Vec<Value> {
    std::fs::read_to_string(perks_file())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

pub fn save_perks(list: &[Value]) {
    let _ = fs::create_dir_all(base_dir());
    let v = Value::Array(list.to_vec());
    let _ = fs::write(perks_file(), serde_json::to_string_pretty(&v).unwrap_or_default());
}

pub fn hidden() -> Vec<String> {
    load_config()
        .get("hidden")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

pub fn set_hidden(app_id: &str, hide: bool) {
    let mut cfg = load_config();
    let mut list: Vec<String> = cfg
        .get("hidden")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    if hide {
        if !list.iter().any(|x| x == app_id) {
            list.push(app_id.to_string());
        }
    } else {
        list.retain(|x| x != app_id);
    }
    cfg["hidden"] = json!(list);
    save_config(&cfg);
}

/// 「显示未安装的程序」开关。默认关：登记表里补进来的那批官方默认路径的产品，
/// 在没装的机器上不该占侧栏格子（见 worth_showing）。
pub fn set_show_uninstalled(on: bool) {
    let mut cfg = load_config();
    cfg["show_uninstalled"] = json!(on);
    save_config(&cfg);
}

pub fn show_uninstalled() -> bool {
    load_config()["show_uninstalled"].as_bool().unwrap_or(false)
}

/// 侧栏展示用：排除被用户屏蔽的条目
/// 一条产品在界面上值不值得占一格：装了、或被自动发现过、或名下已经有账号卡。
/// 2026-10-16 照着 cockpit-tools 的平台清单补进一批（Antigravity / Cursor / Kiro /
/// Windsurf / GitHub Copilot / Zed / Grok / CodeBuddy / Trae 系）之后，这台机器上
/// 大多数都没装——**它们必须被登记**（换台装了的机器要能自动认出来、路径与档案目录都是对的），
/// 但不该在没装的机器上把侧栏塞满。想看得见去 设置 → 高级 → 「显示未安装的程序」。
/// 判据只认三件磁盘事实，不看"登记表里有没有"——否则刚补进来的这批会全部冒出来。
fn worth_showing(a: &AppDef) -> bool {
    if a.discovered {
        return true;
    }
    if resolve_exe(a).is_some() {
        return true;
    }
    std::fs::read_dir(instances_dir(&a.id))
        .map(|rd| {
            rd.filter_map(|e| e.ok()).any(|e| {
                e.path().extension().map(|x| x == "json").unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

pub fn all_apps() -> Vec<AppDef> {
    let h = hidden();
    let show_all = load_config()["show_uninstalled"].as_bool().unwrap_or(false);
    all_apps_raw()
        .into_iter()
        .filter(|a| !h.iter().any(|x| x == &a.id))
        .filter(|a| show_all || worth_showing(a))
        .collect()
}

/// 侧栏收成图标条时用的短名（首字母会撞车：三个 Q）
pub fn short_name(app_id: &str) -> &'static str {
    match app_id {
        "qoder" => "Q",
        "qoder-cn" => "Q中",
        "qoder-cn-ide" => "QIDE",
        "workbuddy" => "WB",
        "workbuddy-intl" => "WAI",
        "astudio" => "AS",
        "minimax" => "MM",
        "traework-cn" => "TC",
        "zcode" => "ZC",
        "codex" => "CX",
        "claude" => "CC",
        "qwenwork" => "QW",
        "dsh" => "DSH",
        "marvis" => "MV",
        "xiaomi-mimo" => "XM",
        "opencode" => "OC",
        // 2026-10-16 补齐的那批：首字母会撞（T/C/Q 各三家），所以都留两三个字母
        "antigravity" => "AG",
        "cursor" => "CU",
        "kiro" => "KI",
        "windsurf" => "WS",
        "github-copilot" => "GH",
        "zed" => "ZD",
        "grok" => "GK",
        "codebuddy" => "CB",
        "codebuddy-cn" => "CB中",
        "trae" => "TR",
        "trae-solo" => "TS",
        "trae-solo-cn" => "TS中",
        _ => "?",
    }
}

/// 自动发现的程序没有预置短名，取名称各词首字母（最多 3 个）
pub fn short_of(app: &AppDef) -> String {
    let fixed = short_name(&app.id);
    if fixed != "?" {
        return fixed.to_string();
    }
    let initials: String = app
        .name
        .split(|c: char| c.is_whitespace() || c == '-' || c == '_')
        .filter(|w| !w.is_empty())
        .take(3)
        .map(|w| w.chars().next().unwrap_or('?'))
        .collect();
    if initials.chars().count() >= 2 {
        initials.to_uppercase()
    } else {
        app.name.chars().take(3).collect::<String>().to_uppercase()
    }
}

pub fn home() -> PathBuf {
    dirs_home().unwrap_or_else(|| PathBuf::from("."))
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// 档案目录可能坐落在的三个顶层位置。换号入口（junction）只会被建在这三处，
/// 迁移数据根时要改的入口链接、以及自动发现探测档案，都按这三层找。
/// APPDATA / LOCALAPPDATA 缺失时（被别的程序唤起的子进程常有这问题）直接跳过，
/// 不能退化成相对路径，那样会把当前工作目录当成档案区。
pub fn profile_search_dirs() -> Vec<PathBuf> {
    let mut out = vec![home()];
    for k in ["APPDATA", "LOCALAPPDATA"] {
        if let Some(v) = std::env::var_os(k) {
            let p = PathBuf::from(v);
            if p.is_absolute() && !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

/// 展开路径模板。`{APPDATA}` / `{LOCALAPPDATA}` / `{USERPROFILE}` 都支持——
/// Qoder 系的登录档案在 `%APPDATA%` 下，不能只认 LOCALAPPDATA。
pub(crate) fn expand(p: &str) -> PathBuf {
    let la = std::env::var("LOCALAPPDATA").unwrap_or_default();
    let ad = std::env::var("APPDATA").unwrap_or_default();
    let up = home().to_string_lossy().into_owned();
    let s = p
        .replace("{LOCALAPPDATA}", &la)
        .replace("{APPDATA}", &ad)
        .replace("{USERPROFILE}", &up);
    // 模板（和 exes 列表）里惯用正斜杠，这里统一落成 Windows 规范形式：
    // 界面上显示、写进 config.json 的路径才不会一半 `\` 一半 `/`
    #[cfg(windows)]
    let s = s.replace('/', "\\");
    PathBuf::from(s)
}

pub fn resolve_exe(app: &AppDef) -> Option<PathBuf> {
    let base = app.exes.iter().map(|p| expand(p)).find(|p| p.exists())?;
    Some(newest_update(&base).unwrap_or(base))
}

/// Qoder 系的更新把新构建放在 `<安装目录>/.qoder-versions/<版本>/<同名 exe>`，顶层那个 exe
/// 可能还是旧版。直接跑顶层会用旧 schema 去开已被新版迁移过的本地库，官方自检弹
/// 「本地数据库版本 85 与当前版本 64 不兼容」然后拒绝启动——所以哪个新跑哪个。
pub fn newest_update(base: &Path) -> Option<PathBuf> {
    let name = base.file_name()?;
    let dir = base.parent()?.join(".qoder-versions");
    let newest = std::fs::read_dir(&dir).ok()?.filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| e.path().join(name))
        .filter(|p| p.is_file())
        .max_by_key(|p| p.metadata().and_then(|m| m.modified()).ok())?;
    let base_t = base.metadata().and_then(|m| m.modified()).ok()?;
    let new_t = newest.metadata().and_then(|m| m.modified()).ok()?;
    (new_t > base_t).then_some(newest)
}

pub fn exe_name(app: &AppDef) -> String {
    PathBuf::from(&app.exes[0])
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 换号模式：Agent 的登录档案目录（junction 的目标）。
/// 手工适配的可以写 `~/.xxx` 相对名、`{APPDATA}/Xxx` 模板或绝对路径；
/// 多个候选用 `|` 分隔——Qoder 0.4.x 换了壳，登录档案挪到了 `%APPDATA%\com.<bundle>.app.stable`
/// （auth.v1.dat），旧的 VS Code 壳那套 `%APPDATA%\<产品>` 可能还留着但早就没人写了。
/// 见 resolve_profile：挑"真的有登录数据"的那个，认错了就换不动登录。
pub fn profile_dir(app_id: &str) -> Option<PathBuf> {
    let app = app_def(app_id)?;
    let spec = app.profile.as_deref()?;
    if spec.trim().is_empty() {
        return None;
    }
    if let Some(p) = adopted_profile_dir(app_id, spec) {
        return Some(p);
    }
    resolve_profile(app_id, spec)
}

/// 这个候选目录上的链接是不是**本产品换号挂上去的入口**。
///
/// 只有目标落在本产品自己的 `instances/<产品>/` 下才算换号——那种必须认它，
/// 否则换号之后解析会漂回另一个"有登录数据"的候选，`swap_clear` 就把备份还到没换过的目录上。
///
/// **指向别处的不算。** Qoder CN 的 `%APPDATA%\QoderCN` 就被一次失败的换号重新接成了
/// 指向同一个真实档案 `com.qodercn.app.stable` 的自指链接（`.cockpit-original` 也还躺在旁边）：
/// `resolve_profile` 每次挑走这个别名 → `shares_one_profile` 又把它当成"已被换号接管" →
/// 卡片掉进「仅可换号」，点切换就去杀整个客户端进程树，而磁盘上一个字节都不需要改。
/// 别名链接必须被看穿，继续按登录数据挑真档案。
///
/// 断链（目标已被删）仍算换号入口：那正是最需要被认出来并修掉的形态（见 swap::self_check）。
fn is_swap_link_in(p: &Path, instances: &Path) -> bool {
    if !is_reparse_point(p) {
        return false;
    }
    match reparse_bound_dir(p) {
        None => true,
        Some(t) => instance_name_under(&t, instances).is_some(),
    }
}

/// 见 `is_swap_link_in`：按本产品自己的 instances 目录判。
pub(crate) fn is_swap_link(app_id: &str, p: &Path) -> bool {
    is_swap_link_in(p, &instances_dir(app_id))
}

fn path_key(p: &str) -> String {
    p.replace('\\', "/").trim_end_matches('/').to_lowercase()
}

/// 「沿用本机原有档案」会把默认目录整个搬进账号目录（记在 seeded_from 里），于是
/// 默认目录本身消失。这时 junction 的目标必须还是**那个空出来的位置**——
/// 否则 resolve_profile 会漂到同产品另一个"还有登录数据"的旧壳目录，
/// 换号换了个没人读的路径，程序照旧起不来。
/// 只在"有候选目录不存在"时才去翻实例档案，正常情况零额外 IO。
fn adopted_profile_dir(app_id: &str, spec: &str) -> Option<PathBuf> {
    let cands: Vec<PathBuf> = spec
        .split('|')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(one_profile)
        .collect();
    if cands.iter().all(|c| c.exists()) {
        return None;
    }
    let src = load_instances(app_id)
        .into_iter()
        .find_map(|m| m["seeded_from"].as_str().map(str::to_string))?;
    pick_adopted(&cands, &[src])
}

fn pick_adopted(cands: &[PathBuf], sources: &[String]) -> Option<PathBuf> {
    let keys: Vec<String> = sources.iter().map(|s| path_key(s)).collect();
    cands
        .iter()
        .find(|c| keys.contains(&path_key(&c.to_string_lossy())))
        .cloned()
}

fn one_profile(p: &str) -> PathBuf {
    if p.starts_with('.') {
        home().join(p)
    } else {
        expand(p)
    }
}

/// 目录是不是 reparse point（junction / symlink）——也就是"已经被换号接管"。
pub(crate) fn is_reparse_point(p: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    std::fs::symlink_metadata(p)
        .map(|m| m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
        .unwrap_or(false)
}

pub(crate) fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(t) => PathBuf::from(t),
        None => p.to_path_buf(),
    }
}

/// 一个 reparse point 真正指向哪个目录；目标已经不在了（断链）就返回 None。
pub(crate) fn reparse_bound_dir(p: &Path) -> Option<PathBuf> {
    if !is_reparse_point(p) {
        return None;
    }
    let t = strip_verbatim(&std::fs::read_link(p).ok()?);
    t.exists().then_some(t)
}

/// 默认档案目录（换号入口）此刻在磁盘上真正指向哪个账号目录。
/// junction 指着的目标已经不存在时返回 None——那就是"断链"，程序会报
/// 「无法创建或写入本地数据目录」，开始菜单图标也会因为读不到
/// application-icons 而变成白纸。
pub(crate) fn bound_instance_dir(app_id: &str) -> Option<PathBuf> {
    reparse_bound_dir(&profile_dir(app_id)?)
}

/// 磁盘上真正绑定的账号名（junction 目标落在 instances/<app>/ 下才算）。
/// UI 上"当前账号"这种状态必须以它为准，否则记录过时会把界面带偏。
pub(crate) fn bound_instance_name(app_id: &str) -> Option<String> {
    if let Some(dir) = bound_instance_dir(app_id) {
        if let Some(n) = instance_name_under(&dir, &instances_dir(app_id)) {
            return Some(n);
        }
    }
    in_place_instance_name(app_id)
}

/// 默认入口是普通目录（没挂链接）时，它是不是某个「原地账号」的本尊。
/// 已经让位的卡不算：单窗口换号的产品只有一份档案，档案里现在登的是新号，
/// 旧号那张卡只是历史记录（released），不能再让它认领这份档案，否则两张卡抢同一个目录。
fn in_place_instance_name(app_id: &str) -> Option<String> {
    let p = profile_dir(app_id)?;
    if is_reparse_point(&p) || !p.is_dir() {
        return None;
    }
    let key = path_key(&p.to_string_lossy());
    load_instances(app_id)
        .into_iter()
        .filter(|m| m["released"].as_bool() != Some(true))
        .find(|m| m["data_dir"].as_str().map(path_key).as_deref() == Some(key.as_str()))
        .and_then(|m| m["name"].as_str().map(str::to_string))
}

/// junction 目标算不算"这个产品名下的账号"。
/// 必须卡父目录：WorkBuddy 和 WorkBuddy AI 是同一产品的两个安装、共用一个档案目录，
/// 不卡就会让其中一个把另一个的账号认成自己的当前账号，把换号记录写脏。
fn instance_name_under(dir: &Path, parent: &Path) -> Option<String> {
    if dir.parent() != Some(parent) {
        return None;
    }
    Some(dir.file_name()?.to_string_lossy().into_owned())
}

/// 这个账号目录是不是**当前真正挂着的那个**默认档案。
/// 只看 config 里的换号记录不够：记录会和磁盘脱节（手动删过目录、改过名、
/// 换号中途失败），照着记录放行删除，就会把默认档案删成断链。
pub(crate) fn is_bound_to(app_id: &str, dir: &Path) -> bool {
    if is_in_place(app_id, dir) {
        // 原地账号：默认入口必须是普通目录。挂着链接说明它正让位给别人。
        return profile_dir(app_id)
            .map(|p| p.is_dir() && !is_reparse_point(&p))
            .unwrap_or(false);
    }
    match bound_instance_dir(app_id) {
        Some(t) => path_key(&t.to_string_lossy()) == path_key(&dir.to_string_lossy()),
        None => false,
    }
}

/// 「原地账号」：这个账号的数据就住在程序的默认档案路径上，没搬家、也没挂链接。
/// 接管本机原有档案本该是这个形态——早期 adopt 把整个目录 rename 进 instances/，
/// 用户天天用的档案因此离开原位，`swaps.original` 还记成空串，
/// 「还原默认」按下去只能还回一个空档案。
pub fn is_in_place(app_id: &str, dir: &Path) -> bool {
    match profile_dir(app_id) {
        Some(p) => path_key(&p.to_string_lossy()) == path_key(&dir.to_string_lossy()),
        None => false,
    }
}

/// 默认档案位置同级那份「换号让位」备份（`xxx.cockpit-original`）的真实路径，没有则空串。
/// 它是「还原默认」唯一的退路，记录丢了就得从磁盘上找回来。
pub fn aside_backup_of(p: &Path) -> String {
    let cand = PathBuf::from(format!("{}.cockpit-original", p.display()));
    cand.is_dir()
        .then(|| cand.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 把「换到这个账号」在磁盘上真正要做的事说清楚。换号按钮与账号设置里那行说明都读它，
/// 免得两处各自猜——猜错的代价是用户只是换个号，却被程序要求把客户端关掉。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapOp {
    /// 默认档案此刻已经就是这个账号：磁盘上没有任何东西可换。
    /// 这类产品所有账号共用一份档案，切换账号＝在客户端里退出登录再登录。
    Nothing,
    /// 要摘/挂 junction，或要把同级让位备份换回来：真动文件，必须先让程序退出。
    Moves,
    /// 这个产品压根没登记可换号的档案目录。
    Unsupported,
}

pub fn swap_op(app_id: &str, dir: &Path) -> SwapOp {
    let p = match profile_dir(app_id) {
        Some(p) => p,
        None => return SwapOp::Unsupported,
    };
    swap_op_for(&p, dir, is_swap_link(app_id, &p), !aside_backup_of(&p).is_empty())
}

/// `swap_op` 的决策表本体（磁盘事实由调用方给），单测钉的是这张表而不是某台机器的状态。
/// `prof_is_link` 是"默认入口挂着**本产品换号**的链接"（见 is_swap_link），
/// 同产品另一个真实档案目录上的别名链接不算——那种换号照样什么都不用动。
fn swap_op_for(prof: &Path, dir: &Path, prof_is_link: bool, has_aside: bool) -> SwapOp {
    if path_key(&prof.to_string_lossy()) != path_key(&dir.to_string_lossy()) {
        return SwapOp::Moves;
    }
    // 目录名相同但默认入口是链接：那是这个账号正被挂成默认档案，
    // 换走它要摘链接、还原还要把真身改回来——仍是要动文件的活
    if prof_is_link || has_aside {
        return SwapOp::Moves;
    }
    SwapOp::Nothing
}

/// 换号是不是空操作（见 `SwapOp::Nothing`）。按磁盘形状判定，不按产品名单硬编码：
/// Qoder 系、WorkBuddy、ZCode、TRAE 这些单窗口产品都会自然落在这里。
pub fn swap_changes_nothing(app_id: &str, dir: &Path) -> bool {
    matches!(swap_op(app_id, dir), SwapOp::Nothing)
}

/// 这个产品是不是「几个账号共用同一份档案」：默认档案位置此刻就是某个账号的本尊——
/// 没挂换号链接、同级也没有让位备份，而且真有一张卡认领着它。
///
/// 四条都不能少。要它不隔离（能多开的产品每个账号有自己的 `--user-data-dir`，
/// 窗口本来分得清，不该按登录态归并）；要它不是 CLI（终端类靠环境变量隔离，另一套）；
/// 默认位置挂着**本产品换号的**链接说明现在正走 junction 换号，那种确实要先关程序
/// （同产品另一个真实档案目录上的**别名**链接不算——那种磁盘上什么都不用动，见 is_swap_link）；
/// 最后还得真有一张卡认领这份档案，否则"共用"无从谈起。
pub fn shares_one_profile(app_id: &str) -> bool {
    let app = match app_def(app_id) {
        Some(a) => a,
        None => return false,
    };
    if app.kind == "cli" || app.isolatable {
        return false;
    }
    let p = match profile_dir(app_id) {
        Some(p) => p,
        None => return false,
    };
    if is_swap_link(app_id, &p) || !aside_backup_of(&p).is_empty() {
        return false;
    }
    // 有没有账号真的住在这份档案上——只看 data_dir 相等，不看「让位」标记：
    // 两张卡都标了让位也不能让产品退回"换号要先关程序"那套，那是错觉
    let key = path_key(&p.to_string_lossy());
    load_instances(app_id)
        .into_iter()
        .any(|m| m["data_dir"].as_str().map(path_key).as_deref() == Some(key.as_str()))
}

/// 这个账号是不是「被搬进 instances/ 的接管账号」，是的话返回它原来的位置。
/// 归位动作与界面入口共用这一条判据，别在两处各写一遍。
pub fn rehome_slot(app_id: &str, meta: &Value) -> Option<PathBuf> {
    let dir = PathBuf::from(meta["data_dir"].as_str()?);
    if dir.parent()? != instances_dir(app_id) {
        return None;
    }
    let seeded = meta["seeded_from"].as_str().unwrap_or("").trim();
    if seeded.is_empty() {
        return None;
    }
    let native = profile_dir(app_id)?;
    (path_key(&native.to_string_lossy()) == path_key(seeded)).then_some(native)
}

/// 从候选里挑真正在用的那个档案目录，优先级：
///   ① **真换号链接**（reparse point 且目标落在本产品 `instances/` 下）—— 那就是换号目标，
///      必须认它。否则换号之后目标目录变成指向空账号的链接、has_login_data 变假，
///      解析会漂回另一个"有登录数据"的候选，还原（swap_clear）就还错了地方。
///      同产品另一个真实档案目录上挂着的**别名**链接不算（见 is_swap_link_in）。
///   ② 有登录数据的候选（按候选顺序）
///   ③ 第一个候选（谁都没有时也得给个目标，新建账号要用）
fn resolve_profile(app_id: &str, spec: &str) -> Option<PathBuf> {
    resolve_profile_in(spec, &instances_dir(app_id))
}

fn resolve_profile_in(spec: &str, instances: &Path) -> Option<PathBuf> {
    let cands: Vec<PathBuf> = spec
        .split('|')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(one_profile)
        .collect();
    let first = cands.first()?.clone();
    if let Some(j) = cands.iter().find(|c| is_swap_link_in(c, instances)) {
        return Some(j.clone());
    }
    for c in &cands {
        if has_login_data(c) {
            return Some(c.clone());
        }
    }
    Some(first)
}

/// 这个目录里有没有"登录数据"。各家标志文件都算：
/// Qoder 新壳 `auth.v1.dat` / 旧壳 `User/globalStorage/state.vscdb`；
/// AStudio 明文 `astron-session.json`；MiniMax `auth/<env>/<region>/mcode-public/`。
/// 2026-10-11 新登记的三家：
///   千问办公 `auth-v2.dat`（Chromium os_crypt v10 信封；同目录 auth.dat 是 v1 旧壳、
///     里面 token 是 "placeholder-" 占位串，不能当登录标志）；
///   DeepSeek Harness `.credentials.yaml`（明文，grant deepseek-account-platform/default）
///     ——dsh 的 profile 是 `.dsh|{APPDATA}/@deepseek-ai/dsh-desktop` 两候选，
///     靠这条把解析钉在真档案 ~/.dsh 上；
///   Marvis `mobile_address.json`（明文 access_token，就在 .marvis symlink 的目标里）。
pub fn has_login_data(dir: &Path) -> bool {
    dir.join("auth.v1.dat").is_file()
        || dir.join("auth-v2.dat").is_file()
        || dir.join("astron-session.json").is_file()
        || dir.join(".credentials.yaml").is_file()
        || dir.join("mobile_address.json").is_file()
        || (dir.join("Local State").is_file()
            && dir.join("User").join("globalStorage").join("state.vscdb").is_file())
        || minimax_credential_files(dir).next().is_some()
}

/// MiniMax 的 mcode 凭据文件（env/region 两段可变，遍历两层）。
fn minimax_credential_files(root: &Path) -> impl Iterator<Item = PathBuf> {
    let auth = root.join("auth");
    let mut out = Vec::new();
    if let Ok(envs) = fs::read_dir(&auth) {
        for e in envs.flatten() {
            if let Ok(regions) = fs::read_dir(e.path()) {
                for r in regions.flatten() {
                    let f = r.path().join("mcode-public").join("auth.json");
                    if f.is_file() {
                        out.push(f);
                    }
                }
            }
        }
    }
    out.into_iter()
}

/// Qoder 这类产品的 Agent 运行时家目录长这样：`.auth` `bin` `ipc` `host-actions`
/// `projects` `tasks` `mcp-router.json`…。它跟"可换号的登录档案"是两回事——
/// 换它既换不动登录（IDE 读的是 %APPDATA%\<产品>），又必然被正在跑的 agent 占住。
/// 这里按标志文件认出来，换号前直接拦住（见 swap.rs）。
const RUNTIME_HOME_MARKERS: &[&str] = &[
    "mcp-router.json",
    ".qoder-app-status.json",
    "host-actions",
    "shell-snapshots",
    "skill-usage.json",
];

/// 命中几个运行时标志才算运行时家目录（要求 ≥2，避免普通档案里恰好有个同名文件）
pub fn looks_like_runtime_home(dir: &Path) -> bool {
    RUNTIME_HOME_MARKERS
        .iter()
        .filter(|m| dir.join(m).exists())
        .count()
        >= 2
}

/// 「一键换号」要存/要写的就是这几个文件：它们决定"从这个入口打开是哪个号"。
/// 条目可以是相对 `profile_dir` 的路径，也可以是带 `{LOCALAPPDATA}` 这类占位符的绝对路径
/// （WorkBuddy 的登录态就不在它自己的档案目录里）。
/// 只列真正含登录态的文件——`auth.machine-id` 这种机器身份绝不能跟着号走，
/// 换了它等于让服务端看见一台新设备。没登记的产品返回空，界面就不摆一键换号。
pub fn auth_pack_files(app_id: &str) -> &'static [&'static str] {
    match app_id {
        // 0.4.x 新壳：os_crypt v10 包着的 {token, refreshToken, expiresAt, user}
        "qoder" | "qoder-cn" => &["auth.v1.dat"],
        // ZCode：一个 JSON 里装着 oauth access_token / jwt / api-key。
        // 里面也夹着这个号自己的 bot 与项目配置——那本来就是它的配置，一起换回去是对的。
        "zcode" => &["v2/credentials.json"],
        // WorkBuddy：登录态不在 ~/.workbuddy，在 CodeBuddy 扩展那份 .info 里。
        // 里面是 $wbEncrypted 信封——我们不解它（解不出来是"读余额"那条路的坎），
        // 换号只要把整份文件原样搬回去就行。
        "workbuddy" => &[
            "{LOCALAPPDATA}/CodeBuddyExtension/Data/Public/auth/workbuddy-desktop.info",
        ],
        "workbuddy-intl" => &[
            "{LOCALAPPDATA}/CodeBuddyExtension/Data/Public/auth/workbuddy-desktop-ai.info",
        ],
        // TRAE CN 故意不做：它的秘密都在 User/globalStorage/state.vscdb 那个 868 KB 的
        // SQLite 里，登录只是其中一小块，整份换会把对话、插件、设置一起换走。
        _ => &[],
    }
}

/// 把一条登录包条目解析成绝对路径：带占位符或以路径分隔符/盘符开头的按模板展开，
/// 其余相对该产品的档案目录。
pub fn auth_pack_path(app_id: &str, entry: &str) -> Option<PathBuf> {    let e = entry.trim();
    if e.is_empty() {
        return None;
    }
    if e.starts_with('{') || e.starts_with('/') || e.starts_with('\\')
        || e.as_bytes().get(1) == Some(&b':')
    {
        return Some(expand(e));
    }
    let dir = profile_dir(app_id)?;
    Some(dir.join(e))
}

/// 这个产品能不能一键换号（登记表里有文件、且路径解析得出来）
pub fn auth_switch_ok(app_id: &str) -> bool {
    auth_pack_files(app_id)
        .iter()
        .any(|e| auth_pack_path(app_id, e).map(|p| p.is_file()).unwrap_or(false))
}

/// 光有登录文件还解不开它——这些是**同一台机器上、跟着档案目录走的辅助文件**，
/// 缺了它们那份登录态就是一堆读不了的密文。按账号铺临时档案目录时要一并带过去。
///
/// Qoder 系：auth.v1.dat 是 os_crypt v10 信封，主密钥由 DPAPI 包在 `Local State` 里。
/// 同一个 Windows 账户下这份主密钥是稳定的，所以把**当前**那份 Local State 借过去，
/// 就能解开**这台机器上任何一个号**存过的 auth.v1.dat。
pub fn auth_pack_support_files(app_id: &str) -> &'static [&'static str] {
    match app_id {
        "qoder" | "qoder-cn" => &["Local State"],
        _ => &[],
    }
}

/// 这个产品的登录态是**按账号各存一份**的吗——只有这种才做得出"按账号查签到"。
/// 判据落在磁盘事实上：登记了登录包文件，且这些文件落在该产品的档案目录**里面**
/// （落在目录外面的，比如 WorkBuddy 那份 %LOCALAPPDATA%\CodeBuddyExtension\…，
///  是按账号铺得出来的，靠 auth_file 走另一条路，见 checkin.rs 的 materialize_kind）。
pub fn per_account_login(app_id: &str) -> bool {
    !auth_pack_files(app_id).is_empty()
}

/// 已知会后台持有登录档案句柄的进程（进程名小写）。
/// 注意 Qoder 国际版与 Qoder CN 是两个 exe，任何一个都可能占着同一个档案空间。
/// 换号时"可能持有该档案句柄"的额外进程名（程序自己的 exe 由 program_pids 覆盖）。
/// **每个产品只能列自己的 exe**：曾经把 qoder 和 qoder-cn 合成一条，结果给国际版换号时
/// 把正在跑的 12 个 `qoder cn.exe` 全当成占用者，CN 开着就永远换不动国际版。
pub fn lockers(app_id: &str) -> &'static [&'static str] {
    match app_id {
        "qoder" => &["qoder.exe"],
        "qoder-cn" | "qoder-cn-ide" => &["qoder cn.exe", "qoder-cn.exe", "qoder cn ide.exe"],
        "codex" => &["chatgpt.exe", "codex.exe"],
        "workbuddy" => &["workbuddy.exe"],
        // 国际版是另一个 exe（WorkBuddyAI.exe）：绝不能把国内版的 workbuddy.exe 算成
        // 国际版的占用者——Qoder 系就踩过这个坑（CN 开着就永远换不动国际版）。
        "workbuddy-intl" => &["workbuddyai.exe"],
        "astudio" => &["astudio.exe"],
        "minimax" => &["minimax code.exe", "minimax.exe"],
        "traework-cn" => &["trae cn.exe", "trae solo cn.exe"],
        "zcode" => &["zcode.exe"],
        // 千问办公：主程序 QwenWorkCN.exe（版本目录里那个）+ 官方自带辅助进程 qwenworkhelper.exe，
        // 都是自家名字；启动壳 Launcher.exe 是短命的，不用列。
        "qwenwork" => &["qwenworkcn.exe", "qwenworkhelper.exe"],
        "dsh" => &["deepseek harness.exe"],
        // Marvis 自家进程族（版本目录下 Marvis*.exe），只认 marvis 前缀的名字。
        "marvis" => &["marvis.exe", "marvislauncher.exe", "marvissvr.exe", "marvisassistant.exe"],
        "xiaomi-mimo" => &["xiaomi mimo.exe"],
        "opencode" => &["opencode.exe"],
        _ => &[],
    }
}

pub const CLEAN_DIRS: &[&str] = &[
    "Cache",
    "Code Cache",
    "GPUCache",
    "logs",
    "Crashpad",
    "DawnCache",
    "DawnGraphiteCache",
    "DawnWebGPUCache",
    "ShaderCache",
    "Service Worker",
    "CachedData",
    "CachedExtensionVSIXs",
    "CachedProfilesData",
    "tmp",
    "CrashFeedback",
    "Backups",
];

pub const BACKUP_SKIP: &[&str] = &[
    "Cache",
    "Code Cache",
    "GPUCache",
    "logs",
    "Crashpad",
    "DawnCache",
    "ShaderCache",
    "Service Worker",
];

/// 驾驶舱数据根。真身在哪由 storage 一层解析（环境变量 > 指针文件 > 默认家目录），
/// 这里只负责转发——**别在任何地方再算一遍**，否则迁移后新旧两处都会被人当成本尊。
pub fn base_dir() -> PathBuf {
    crate::storage::resolve_root()
}
pub fn instances_dir(app_id: &str) -> PathBuf {
    base_dir().join("instances").join(app_id)
}
pub fn backup_dir() -> PathBuf {
    base_dir().join("backups")
}
pub fn config_file() -> PathBuf {
    base_dir().join("config.json")
}

pub fn ensure_dirs() {
    let _ = fs::create_dir_all(instances_dir(""));
    let _ = fs::create_dir_all(backup_dir());
}

/// 账号自带 token（抓额度用的）在保管箱里的编号
pub fn token_id(app_id: &str, name: &str) -> String {
    format!("token:{}:{}", app_id, name)
}

/// 把各账号文档里残留的明文 token 收进保管箱（启动时、以及解锁之后各跑一次）。
/// 走 save_instance 这一个写出口，判据不和别处重复。
pub fn sweep_tokens() -> usize {
    if !crate::secrets::available() {
        return 0;
    }
    let mut n = 0usize;
    for app in all_apps_raw() {
        let id = app.id.as_str();
        for m in load_instances(id) {
            let token = m["token"].as_str().unwrap_or("").trim();
            if !token.is_empty() {
                save_instance(id, &m);
                n += 1;
            }
        }
    }
    n
}

pub fn load_instances(app_id: &str) -> Vec<Value> {
    let dir = instances_dir(app_id);
    let mut out = Vec::new();
    let rd = match fs::read_dir(&dir) {
        Ok(r) => r,
        Err(_) => return out,
    };
    let mut files: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "json").unwrap_or(false))
        .collect();
    files.sort();
    for f in files {
        if let Ok(s) = fs::read_to_string(&f) {
            if let Ok(mut v) = serde_json::from_str::<Value>(&s) {
                // token 存在保管箱里，读出来补回字段，调用方照旧用 meta["token"]
                if v["token"].as_str().unwrap_or("").is_empty() {
                    let name = v["name"].as_str().unwrap_or("").to_string();
                    if let Some(t) = crate::secrets::get(&token_id(app_id, &name)) {
                        v["token"] = json!(t);
                    }
                }
                v["_file"] = json!(f.to_string_lossy());
                out.push(v);
            }
        }
    }
    out
}

pub fn find_instance(app_id: &str, name: &str) -> Option<Value> {
    load_instances(app_id)
        .into_iter()
        .find(|m| m["name"].as_str() == Some(name))
}

pub fn save_instance(app_id: &str, meta: &Value) {
    let dir = instances_dir(app_id);
    let _ = fs::create_dir_all(&dir);
    if let Some(name) = meta["name"].as_str() {
        // 账号名直接拼进文件名，必须挡住路径分隔符，防止越界写盘
        if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
            return;
        }
        let mut clean = meta.as_object().cloned().unwrap_or_default();
        clean.remove("_file");
        // token 落盘前先收进保管箱。保管箱锁着（有主密码还没解锁）时不能删字段，
        // 也不能把"补不出来的空值"当成用户清空——那会把真凭据抹掉。
        let token = clean
            .get("token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let held = crate::secrets::available();
        if !token.is_empty() {
            if held && crate::secrets::put(&token_id(app_id, name), &token) {
                clean.remove("token");
                clean.insert("token_held".into(), json!(true));
                let _ = crate::secrets::flush();
            }
        } else if held {
            clean.remove("token_held");
            crate::secrets::remove(&token_id(app_id, name));
            let _ = crate::secrets::flush();
        }
        let v = Value::Object(clean);
        let _ = fs::write(
            dir.join(format!("{}.json", name)),
            serde_json::to_string_pretty(&v).unwrap_or_default(),
        );
    }
}

/// 一个账号可能出现在进程命令行里的所有路径写法：
/// 账号目录本身、它的 canonical 路径、以及"该账号被设为当前"时的默认档案目录（junction 场景）
pub fn account_candidates(app_id: &str, meta: &Value, swaps: &Value) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    let mut push = |s: String| {
        let s = s.trim_end_matches('\\').to_lowercase();
        if !s.is_empty() && !v.contains(&s) {
            v.push(s);
        }
    };
    if let Some(d) = meta["data_dir"].as_str() {
        push(d.to_string());
        if let Ok(c) = std::fs::canonicalize(d) {
            let s = c.to_string_lossy().replace("\\\\?\\", "");
            push(s);
        }
    }
    let swapped_here = swaps
        .get(app_id)
        .and_then(|s| s["instance"].as_str())
        .map(|i| i == meta["name"].as_str().unwrap_or(""))
        .unwrap_or(false);
    if swapped_here {
        if let Some(p) = profile_dir(app_id) {
            push(p.to_string_lossy().into_owned());
        }
    }
    v
}

/// 最后一次成功解析的账本。load_config 万一读到半截文件（进程被强杀、磁盘满），
/// 退回这一份，**绝不退回空默认值**——那等于把 instances/swaps/perks 全抹了。
fn last_good() -> &'static std::sync::Mutex<Option<Value>> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<std::sync::Mutex<Option<Value>>> = OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(None))
}

/// 账本文本的解析规则，单拎出来才能测（真文件路径不能碰）。
/// 解析不出来时退回上一份好数据；**没有上一份才用默认值**——
/// 直接返回默认值等于把 instances/swaps/perks 全清空，下一次保存就写回去了。
fn parse_config(text: &str, last_good: Option<&Value>) -> Option<Value> {
    match serde_json::from_str::<Value>(text) {
        Ok(v) if v.is_object() => Some(v),
        _ => last_good.cloned(),
    }
}

pub fn load_config() -> Value {
    let text = fs::read_to_string(config_file()).unwrap_or_default();
    let cached = last_good().lock().ok().and_then(|c| c.clone());
    if let Some(v) = parse_config(&text, cached.as_ref()) {
        if let Ok(mut c) = last_good().lock() {
            *c = Some(v.clone());
        }
        return v;
    }
    json!({"proxy": {"upstream": "https://api.openai.com", "strategy": "round-robin"}})
}

/// 先写临时文件再改名覆盖：fs::write 会把文件截断，另一边正在读的话
/// 就会拿到半截 JSON，解析失败后一次保存就能把账本清空。
pub fn save_config(cfg: &Value) {
    let _ = fs::create_dir_all(base_dir());
    let text = match serde_json::to_string_pretty(cfg) {
        Ok(t) => t,
        Err(_) => return,
    };
    let dst = config_file();
    let tmp = dst.with_extension("json.tmp");
    if fs::write(&tmp, &text).is_ok() {
        // Windows 上 std 的 rename 带 REPLACE_EXISTING，可以直接盖过去
        if fs::rename(&tmp, &dst).is_ok() {
            if let Ok(mut c) = last_good().lock() {
                *c = Some(cfg.clone());
            }
            return;
        }
        let _ = fs::remove_file(&tmp);
    }
    // 改名失败（被占用等）也要尽力落盘，别让改动整个丢掉
    let _ = fs::write(&dst, &text);
    if let Ok(mut c) = last_good().lock() {
        *c = Some(cfg.clone());
    }
}

pub fn swaps_of(cfg: &Value) -> Value {
    cfg.get("swaps").cloned().unwrap_or_else(|| json!({}))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 半截 JSON / 空文件必须退回上一份好数据，不能退回空默认值
    #[test]
    fn torn_config_falls_back_to_last_good_not_to_empty() {
        let good = json!({ "instances": { "qoder-cn": ["账号1"] }, "swaps": { "workbuddy": {} } });
        // 读到截断内容：拿回上一份
        let got = parse_config("{\"instances\":", Some(&good)).expect("该退回上一份好数据");
        assert_eq!(got["instances"]["qoder-cn"][0], json!("账号1"));
        // 空文件同理
        assert!(parse_config("", Some(&good)).is_some());
        // 数组/标量这种"能解析但不是账本"的内容也不算好数据
        assert!(parse_config("[1,2]", Some(&good)).is_some());
        assert_eq!(parse_config("[1,2]", Some(&good)).unwrap(), good);
        // 第一次启动、连上一份都没有 → None，由调用方决定用默认值
        assert!(parse_config("garbage", None).is_none());
        // 正常内容原样返回
        let ok = parse_config("{\"a\":1}", Some(&good)).unwrap();
        assert_eq!(ok["a"], json!(1));
    }

    /// adopt（沿用原有档案）把默认目录搬走后，junction 目标必须还是那个空出来的位置，
    /// 不能漂到同产品另一个"还有登录数据"的旧壳目录上
    #[test]
    fn adopted_dir_stays_the_junction_target() {
        let new_shell = PathBuf::from(r"C:\Users\x\AppData\Roaming\com.qoder.app.stable");
        let legacy = PathBuf::from(r"C:\Users\x\AppData\Roaming\Qoder");
        let cands = [new_shell.clone(), legacy];
        let moved = vec![r"C:\Users\x\AppData\Roaming\com.qoder.app.stable".to_string()];
        assert_eq!(pick_adopted(&cands, &moved).as_deref(), Some(new_shell.as_path()));
        // 反斜杠/大小写/尾部分隔符都得认
        let moved2 = vec![r"C:\USERS\x\AppData\Roaming\com.qoder.app.stable\".to_string()];
        assert!(pick_adopted(&cands, &moved2).is_some());
        assert!(pick_adopted(&cands, &[]).is_none(), "没搬过家就别插手");
        assert!(
            pick_adopted(&cands, &["C:\\Users\\x\\AppData\\Roaming\\Other".to_string()]).is_none(),
            "搬的是别的目录，不该认领候选"
        );
    }

    /// 顶层 exe 是旧构建时要用 `.qoder-versions` 里更新过的那个：
    /// 跑旧的会拿旧 schema 去开被新版迁移过的本地库，官方直接拒绝启动
    #[test]
    fn newest_update_prefers_updated_build() {
        let tmp = std::env::temp_dir().join("cockpit-newest-update-test");
        let _ = fs::remove_dir_all(&tmp);
        let root = tmp.join("Qoder.exe");
        fs::create_dir_all(&tmp).unwrap();
        fs::write(&root, b"old").unwrap();
        assert_eq!(newest_update(&root), None, "没有 .qoder-versions 时用顶层");

        let v1 = tmp.join(".qoder-versions").join("0.4.1");
        let v3 = tmp.join(".qoder-versions").join("0.4.3");
        fs::create_dir_all(&v1).unwrap();
        fs::create_dir_all(&v3).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        fs::write(v1.join("Qoder.exe"), b"0.4.1").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        let newest = v3.join("Qoder.exe");
        fs::write(&newest, b"0.4.3").unwrap();
        assert_eq!(newest_update(&root).as_deref(), Some(newest.as_path()));

        // 顶层被重新装过、比版本目录还新 → 别再去翻旧的版本目录
        std::thread::sleep(std::time::Duration::from_millis(30));
        fs::write(&root, b"reinstalled").unwrap();
        assert_eq!(newest_update(&root), None);
        let _ = fs::remove_dir_all(&tmp);
    }

    /// 真机才跑：本机 Qoder 国际版做过「沿用原有档案」（默认目录被搬进账号目录），
    /// 这时换号目标必须还是 com.qoder.app.stable 那个空出来的位置，
    /// 不能漂到旧壳 %APPDATA%\Qoder —— 漂了就换错地方，程序照旧起不来
    #[test]
    #[ignore]
    fn qoder_profile_targets_the_live_shell() {
        let p = profile_dir("qoder").expect("qoder 该有档案目录");
        let s = p.to_string_lossy().replace('\\', "/").to_lowercase();
        assert!(
            s.ends_with("com.qoder.app.stable"),
            "换号目标错了：{}（旧壳 %APPDATA%\\Qoder 没人读，adopt 之后更不该选它）",
            p.display()
        );
    }

    /// 换号账号的运行状态靠"默认档案目录"匹配，这里验证候选路径集合的正确性
    #[test]
    fn account_candidates_cover_junction_case() {
        let tmp = std::env::temp_dir().join("cockpit-cand-test");
        let _ = fs::create_dir_all(&tmp);
        let dir = tmp.join("DuckDeity");
        let _ = fs::create_dir_all(&dir);
        let meta = json!({ "name": "DuckDeity", "data_dir": dir.to_string_lossy() });

        // 未换号：只有账号目录本身
        let c = account_candidates("workbuddy", &meta, &json!({}));
        assert!(c.iter().any(|s| s.ends_with("duckdeity")), "应含账号目录: {:?}", c);
        assert!(c.iter().all(|s| !s.contains(".workbuddy")), "未换号不该含默认档案");

        // 已换号：默认档案目录也要进候选（进程命令行里出现的是它）
        let swaps = json!({ "workbuddy": { "instance": "DuckDeity" } });
        let c2 = account_candidates("workbuddy", &meta, &swaps);
        assert!(
            c2.iter().any(|s| s.contains(".workbuddy")),
            "换号后应含默认档案目录: {:?}",
            c2
        );
        // 别人的换号记录不该混进来
        let c3 = account_candidates("workbuddy", &meta, &json!({ "workbuddy": { "instance": "别人" } }));
        assert!(c3.iter().all(|s| !s.contains(".workbuddy")), "不该误认成当前账号");

        // 统一小写、去尾部分隔符
        assert!(c2.iter().all(|s| s == &s.to_lowercase() && !s.ends_with('\\')));
        let _ = fs::remove_dir_all(&tmp);
    }

    /// Qoder 的登录档案在 0.4.x 换壳后有两个可能位置，profile 写的是候选列表；
    /// 必须挑"真有登录数据"的那个——认错了要么读到几个月前的旧账，要么换号换不动登录
    #[test]
    fn profile_candidates_pick_the_dir_with_login_data() {
        let tmp = std::env::temp_dir().join("cockpit-profile-cands-test");
        let _ = fs::remove_dir_all(&tmp);
        let instances = tmp.join("instances");
        let new_shell = tmp.join("com.qoder.app.stable");
        let legacy = tmp.join("Qoder");
        fs::create_dir_all(&new_shell).unwrap();
        fs::create_dir_all(&legacy).unwrap();

        let spec = format!("{}|{}", new_shell.to_string_lossy(), legacy.to_string_lossy());
        // 谁都没有登录数据 → 取第一个候选（新建账号时也得有个目标）
        assert_eq!(
            resolve_profile_in(&spec, &instances).as_deref(),
            Some(new_shell.as_path())
        );

        // 只有旧壳有 state.vscdb → 选旧壳
        fs::create_dir_all(legacy.join("User").join("globalStorage")).unwrap();
        fs::write(legacy.join("Local State"), "{}").unwrap();
        fs::write(legacy.join("User").join("globalStorage").join("state.vscdb"), "").unwrap();
        assert_eq!(
            resolve_profile_in(&spec, &instances).as_deref(),
            Some(legacy.as_path())
        );
        assert!(has_login_data(&legacy));

        // 新壳也有 auth.v1.dat → 按候选顺序优先新壳（0.4.3 就是它）
        fs::write(new_shell.join("auth.v1.dat"), b"v10xx").unwrap();
        assert_eq!(
            resolve_profile_in(&spec, &instances).as_deref(),
            Some(new_shell.as_path())
        );
        assert!(has_login_data(&new_shell));
        // 只有 Local State 还不算登录过
        assert!(!has_login_data(&new_shell.join("nonexistent")));

        // 单个候选（不带 `|`）照旧可用
        let one = legacy.to_string_lossy().to_string();
        assert_eq!(
            resolve_profile_in(&one, &instances).as_deref(),
            Some(legacy.as_path())
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    /// Qoder 系换号目标绝不能是 agent 的运行时家目录（~/.qoder）：
    /// 那是被会话占着的目录，rename 必然「拒绝访问」
    #[test]
    fn qoder_profile_is_not_the_agent_runtime_home() {
        for (id, ok_ends) in [("qoder", ["com.qoder.app.stable", "Qoder"]),
                              ("qoder-cn", ["com.qodercn.app.stable", "QoderCN"])] {
            let d = profile_dir(id).expect("Qoder 系该有换号目标");
            let s = d.to_string_lossy().into_owned();
            // 注意 `com.qoder.app.stable` 本身含 ".qoder"，所以按路径分量判
            let runtime = d.components().any(|c| c.as_os_str() == ".qoder");
            assert!(!runtime, "{id} 的换号目标不该落在运行时家目录：{d:?}");
            assert_ne!(d, home().join(".qoder"));
            assert!(d.is_absolute(), "{id} 的换号目标该是绝对路径：{d:?}");
            assert!(
                ok_ends.iter().any(|e| s.ends_with(e)),
                "{id} 的换号目标看着不对：{d:?}"
            );
        }
    }

    /// 换号到底要不要动文件——这张表决定驾驶舱有没有资格要求用户关掉客户端。
    /// 共用一份档案的产品（Qoder 系 / WorkBuddy / ZCode / TRAE…）账号目录就是默认档案本身，
    /// 切过去一个字节都不改，却曾被按"换号"流程去 kill 整个程序的进程树。
    #[test]
    fn switching_to_the_shared_profile_itself_moves_nothing() {
        let prof = std::path::Path::new("C:/Users/x/AppData/Roaming/com.qodercn.app.stable");
        // 目标 == 默认档案、没挂链接、同级没让位备份：空操作
        assert_eq!(
            swap_op_for(prof, prof, false, false),
            SwapOp::Nothing,
            "共用一份档案的换号不该要求关程序"
        );
        // 路径只差大小写与尾斜杠，仍是同一份
        let same = std::path::Path::new("C:\\users\\x\\AppData\\Roaming\\com.qodercn.app.stable\\");
        assert_eq!(swap_op_for(prof, same, false, false), SwapOp::Nothing);
        // 别的目录（instances/ 下的账号档案）：要挂 junction，是真活
        let other = std::path::Path::new("D:/AgentData/.agent-cockpit/instances/qoder-cn/账号2");
        assert_eq!(swap_op_for(prof, other, false, false), SwapOp::Moves);
        // 同名但默认入口现在是 junction（这个号正被挂着）：换走要摘链接
        assert_eq!(swap_op_for(prof, prof, true, false), SwapOp::Moves);
        // 同级躺着让位备份：换回来要改名
        assert_eq!(swap_op_for(prof, prof, false, true), SwapOp::Moves);
    }

    /// TRAE CN（原 TraeWork）的换号目标必须是 userData（%APPDATA% 下的登录目录，
    /// storage.json 在那），不是 ~/.trae-cn——那是它的运行时数据目录（对话历史
    /// assistant / 插件 plugins / skills / mcps），换它既换不动登录，
    /// 又可能把正在用的插件和对话记录搬乱。
    /// 2026-10-11 升级把目录从 TRAE SOLO CN 改名 Trae CN：新版优先，老安装回落旧目录。
    #[test]
    fn trae_profile_points_at_userdata_not_runtime_home() {
        let d = profile_dir("traework-cn").expect("TRAE CN 该有换号目标");
        let s = d.to_string_lossy().to_lowercase();
        assert!(
            s.ends_with("trae cn") || s.ends_with("trae solo cn"),
            "TRAE CN 的换号目标该是 %APPDATA% 下的登录目录（Trae CN 优先、TRAE SOLO CN 回落），实际：{s}"
        );
        assert!(d.is_absolute(), "该是绝对路径：{d:?}");
        assert_ne!(d, home().join(".trae-cn"));
        // 运行时数据目录 ~/.trae-cn 不该被当成换号目标
        assert!(!d.components().any(|c| c.as_os_str() == ".trae-cn"));
        // 有登录数据（Local State + state.vscdb），has_login_data 该认得出
        assert!(has_login_data(&d), "userData 目录该有登录数据标志：{d:?}");
    }

    /// 换号后目标目录变成 junction，解析必须继续认它——否则会漂回另一个"有登录数据"的
    /// 候选目录，swap_clear 就还原到错的地方（把备份还到没换过的那个目录上）
    #[test]
    fn swapped_junction_wins_over_other_candidates_with_login() {
        let tmp = std::env::temp_dir().join("cockpit-junction-resolve-test");
        let _ = fs::remove_dir_all(&tmp);
        let instances = tmp.join("instances");
        let new_shell = tmp.join("com.qoder.app.stable");
        let legacy = tmp.join("Qoder");
        // 换号链接的目标必须是本产品 instances/ 下的账号目录（is_swap_link 的判据）
        let inst = instances.join("账号1");
        for d in [&new_shell, &legacy, &inst] {
            fs::create_dir_all(d).unwrap();
        }
        // 旧壳那份有登录数据，新壳是空的 → 没换号时该选旧壳
        fs::create_dir_all(legacy.join("User").join("globalStorage")).unwrap();
        fs::write(legacy.join("Local State"), "{}").unwrap();
        fs::write(legacy.join("User").join("globalStorage").join("state.vscdb"), "").unwrap();
        let spec = format!(
            "{}|{}",
            new_shell.to_string_lossy(),
            legacy.to_string_lossy()
        );
        assert_eq!(
            resolve_profile_in(&spec, &instances).as_deref(),
            Some(legacy.as_path())
        );

        // 把新壳那个目录换成指向账号目录的 junction（就是"已换号"的状态）→ 解析必须认它
        let _ = fs::remove_dir(&new_shell);
        let st = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &new_shell.to_string_lossy(),
                &inst.to_string_lossy(),
            ])
            .output()
            .unwrap();
        assert!(
            st.status.success(),
            "mklink /J 失败：{}",
            String::from_utf8_lossy(&st.stdout)
        );
        assert!(is_reparse_point(&new_shell), "该是 reparse point");
        assert!(is_swap_link_in(&new_shell, &instances), "目标在 instances/ 下，算换号链接");
        assert_eq!(
            resolve_profile_in(&spec, &instances).as_deref(),
            Some(new_shell.as_path())
        );

        // 即便这时旧壳那份仍然是"有登录数据"的，也不能把它选走
        assert!(has_login_data(&legacy));
        assert_eq!(
            resolve_profile_in(&spec, &instances).as_deref(),
            Some(new_shell.as_path())
        );
        let _ = fs::remove_dir(&new_shell);
        let _ = fs::remove_dir_all(&tmp);
    }

    /// 别名链接不算换号：`%APPDATA%\QoderCN` 曾被一次失败的换号重新接成了指向同一个
    /// 真实档案 `com.qodercn.app.stable` 的自指链接，`.cockpit-original` 也还躺在旁边。
    /// 以前 resolve_profile 只要看见 reparse point 就选它，于是 profile_dir 变成那个别名，
    /// `shares_one_profile` 又因为"默认入口挂着链接"判成"已被换号接管"——
    /// 卡片掉进「仅可换号」，点「切到本账号并打开」就去 kill 整个 qoder.cn.exe 进程树，
    /// 而磁盘上一个字节都不需要改。别名必须被看穿，继续按登录数据挑真档案。
    #[test]
    fn alias_link_does_not_hijack_the_real_profile() {
        let tmp = std::env::temp_dir().join("cockpit-alias-link-test");
        let _ = fs::remove_dir_all(&tmp);
        let instances = tmp.join("instances");
        let real = tmp.join("com.qodercn.app.stable");
        let alias = tmp.join("QoderCN");
        fs::create_dir_all(&real).unwrap();
        fs::create_dir_all(&instances).unwrap();
        // 真档案里有登录数据（0.4.x 新壳就写这儿）
        fs::write(real.join("auth.v1.dat"), b"v10xx").unwrap();
        fs::write(real.join("Local State"), "{}").unwrap();
        // 别名：指向同一个真档案的链接（不是换号——目标是同级的真档案，不在 instances/ 下）
        let st = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J", &alias.to_string_lossy(), &real.to_string_lossy()])
            .output()
            .unwrap();
        assert!(st.status.success(), "mklink /J 失败：{}", String::from_utf8_lossy(&st.stdout));
        assert!(is_reparse_point(&alias));
        assert!(!is_swap_link_in(&alias, &instances), "指向同级真档案的别名不是换号链接");

        let spec = format!("{}|{}", real.to_string_lossy(), alias.to_string_lossy());
        assert_eq!(
            resolve_profile_in(&spec, &instances).as_deref(),
            Some(real.as_path()),
            "别名不该把真档案挤走（挤走了这张卡就会掉进「仅可换号」）"
        );

        // 断链仍是换号入口：目标已被删的那种最需要被认出来并修掉
        let dangling = tmp.join("QoderCN-dangling");
        let gone = tmp.join("no-such-account");
        let st = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J", &dangling.to_string_lossy(), &gone.to_string_lossy()])
            .output()
            .unwrap();
        assert!(st.status.success(), "mklink /J 失败：{}", String::from_utf8_lossy(&st.stdout));
        assert!(is_swap_link_in(&dangling, &instances), "断链该被认成换号入口");
        let _ = fs::remove_dir(&alias);
        let _ = fs::remove_dir(&dangling);
        let _ = fs::remove_dir_all(&tmp);
    }

    /// 真机才跑（`cargo test -- --include-ignored`）：这台机器上 Qoder CN 的默认入口
    /// `%APPDATA%\QoderCN` 是一次失败换号留下的**自指别名链接**（指向同一个真实档案
    /// `com.qodercn.app.stable`，`.cockpit-original` 还躺在旁边）。
    /// 别名不许把真档案挤走，也不许把产品判成「仅可换号」——掉进那条分支之后，
    /// 点「切到本账号并打开」会去 kill 整个 qoder.cn.exe 进程树，而磁盘上一个字节都不需要改。
    #[test]
    #[ignore]
    fn real_machine_qoder_cn_is_a_shared_profile_not_a_swap() {
        let d = profile_dir("qoder-cn").expect("Qoder CN 该有档案目录");
        assert!(
            d.to_string_lossy().ends_with("com.qodercn.app.stable"),
            "别名链接不该把真档案挤走：{d:?}"
        );
        assert!(!is_swap_link("qoder-cn", &d), "真档案本身不该被认成换号链接：{d:?}");
        assert!(shares_one_profile("qoder-cn"), "它就该被认成「账号共用一份档案」");
        let inst = load_instances("qoder-cn");
        let dir = inst
            .first()
            .and_then(|m| m["data_dir"].as_str())
            .map(std::path::PathBuf::from)
            .expect("这台机器上该有 Qoder CN 的账号卡");
        assert!(swap_changes_nothing("qoder-cn", &dir), "切到共用档案不该动任何文件");
    }

    fn mk_junction(link: &Path, target: &Path) {
        let st = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &link.to_string_lossy(),
                &target.to_string_lossy(),
            ])
            .output()
            .unwrap();
        assert!(
            st.status.success(),
            "mklink /J 失败：{}",
            String::from_utf8_lossy(&st.stdout)
        );
    }

    /// 断链（junction 指向已经不存在的账号目录）必须判成"没绑定"。
    /// 判错的话：换号会以为"已经是它"而不去修链接，删除也不会先还原默认——
    /// Qoder 国际版那次「无法创建或写入本地数据目录」+ 开始菜单图标变白纸就是这么留下的。
    #[test]
    fn dangling_junction_is_not_a_binding() {
        let tmp = std::env::temp_dir().join("cockpit-dangling-junction-test");
        let _ = fs::remove_dir_all(&tmp);
        let link = tmp.join("com.qoder.app.stable");
        let inst = tmp.join("账号1");
        fs::create_dir_all(&link).unwrap();
        fs::create_dir_all(&inst).unwrap();
        fs::remove_dir(&link).unwrap();
        mk_junction(&link, &inst);
        assert_eq!(reparse_bound_dir(&link).as_deref(), Some(inst.as_path()));

        fs::remove_dir_all(&inst).unwrap();
        assert_eq!(reparse_bound_dir(&link), None, "目标没了就不算绑定");
        assert!(is_reparse_point(&link), "链接本身还挂在那儿");

        let _ = fs::remove_dir_all(&tmp);
    }

    /// junction 目标只有落在**这个产品自己的** instances/<app>/ 下才算它的当前账号。
    /// WorkBuddy 与 WorkBuddy AI 共用一个档案目录，不卡父目录就会互相把对方的账号
    /// 认成自己的，然后 self_check 把换号记录写脏。
    #[test]
    fn bound_name_only_counts_instances_under_this_app() {
        let inst = Path::new(r"C:\Users\x\.agent-cockpit\instances");
        let zcode = inst.join("zcode");
        assert_eq!(
            instance_name_under(&zcode.join("shen1950"), &zcode).as_deref(),
            Some("shen1950")
        );
        assert_eq!(instance_name_under(&inst.join("workbuddy/DuckDeity"), &zcode), None);
        assert_eq!(instance_name_under(Path::new("D:\\Somewhere\\Else"), &zcode), None);
        assert_eq!(instance_name_under(&zcode, &zcode), None, "产品目录本身不是账号");
    }

    /// Qoder 的登录档案在 %APPDATA%\<产品>，不能只认 LOCALAPPDATA——
    /// 认错了就换不动登录（打开还是原来那个号）
    #[test]
    fn appdata_template_is_expanded_for_qoder_profiles() {
        let ad = std::env::var("APPDATA").unwrap_or_default();
        assert!(!ad.is_empty(), "本机该有 APPDATA");
        let p = expand("{APPDATA}/Qoder");
        assert!(p.to_string_lossy().ends_with("AppData\\Roaming\\Qoder")
            || p.to_string_lossy().ends_with("AppData/Roaming/Qoder"), "实际：{p:?}");

        let q = app_def("qoder").expect("qoder 该在 curated 里");
        assert_eq!(q.profile.as_deref(), Some("{APPDATA}/com.qoder.app.stable|{APPDATA}/Qoder"));
        // 0.4.x 新壳的目录里就有 auth.v1.dat，所以解析结果是它
        let d = profile_dir("qoder").unwrap();
        assert_eq!(d, PathBuf::from(&ad).join("com.qoder.app.stable"));
        // 一定不能是 ~/.qoder（那是 agent 运行时家目录，换它既无效又被占）
        assert_ne!(d, home().join(".qoder"));
        assert!(!d.components().any(|c| c.as_os_str() == ".qoder"));
    }

    /// `~/.qoder` 不是可换号的登录档案，要能被认出来并拦住
    #[test]
    fn runtime_home_is_recognized_and_refused() {
        let tmp = std::env::temp_dir().join("cockpit-runtime-home-test");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        // 空目录 / 只有 1 个标志文件：不算
        assert!(!looks_like_runtime_home(&tmp));
        fs::write(tmp.join("mcp-router.json"), "{}").unwrap();
        assert!(!looks_like_runtime_home(&tmp), "只命中 1 个不算，避免误伤普通档案");

        // 真实的 ~/.qoder：.auth bin ipc host-actions projects tasks mcp-router.json …
        fs::write(tmp.join(".qoder-app-status.json"), "{}").unwrap();
        fs::create_dir_all(tmp.join("host-actions")).unwrap();
        assert!(looks_like_runtime_home(&tmp), "命中 3 个该判为运行时家目录");

        // 真正的登录档案（Local State + state.vscdb）不该被误判
        let pf = std::env::temp_dir().join("cockpit-real-profile-test");
        let _ = fs::remove_dir_all(&pf);
        fs::create_dir_all(pf.join("User").join("globalStorage")).unwrap();
        fs::write(pf.join("Local State"), "{}").unwrap();
        fs::write(pf.join("User").join("globalStorage").join("state.vscdb"), "").unwrap();
        assert!(!looks_like_runtime_home(&pf));

        let _ = fs::remove_dir_all(&tmp);
        let _ = fs::remove_dir_all(&pf);
    }

    /// 换号前要能点出"谁占着"：Qoder 系的 locker 曾经错抄成 codex 的 exe 名，
    /// 结果"强制换号"杀不掉真正的占用者，rename 照样拒绝访问。
    /// 反过来也不能串：qoder 和 qoder-cn 共用一张名单时，给国际版换号会把正在跑的
    /// 12 个 `qoder cn.exe` 全列成占用者——CN 开着就永远换不动国际版。
    /// WorkBuddy 与国内版/国际版也不能互串：国际版的 exe 是 WorkBuddyAI.exe，
    /// **绝不能把国内版的 WorkBuddy.exe 算成国际版的占用者**（这条 curated 早先整条
    /// 抄了国内版，就是这个坑的档案版；现在三处——exes / profile / lockers——全部拆开）。
    /// 新登记的四家（qwenwork/dsh/marvis/xiaomi-mimo）也各自只认自家 exe。
    #[test]
    fn lockers_stay_per_product() {
        let q = lockers("qoder");
        assert!(q.contains(&"qoder.exe"), "qoder 该认自己的 exe：{q:?}");
        assert!(!q.contains(&"qoder cn.exe"), "qoder 不该把 CN 的进程算成占用者：{q:?}");
        let cn = lockers("qoder-cn");
        assert!(cn.contains(&"qoder cn.exe"), "qoder-cn 该认自己的 exe：{cn:?}");
        assert!(!cn.contains(&"qoder.exe"), "qoder-cn 不该把国际版的进程算成占用者：{cn:?}");
        for id in ["qoder", "qoder-cn"] {
            let l = lockers(id);
            assert!(!l.contains(&"codex.exe"), "{id} 不该带 codex 的 exe：{l:?}");
            assert!(!l.contains(&"chatgpt.exe"), "{id} 不该带 chatgpt 的 exe：{l:?}");
        }
        assert_eq!(lockers("codex"), &["chatgpt.exe", "codex.exe"]);
        assert!(lockers("不存在的产品").is_empty());
    }

    /// 新登记几家：档案目录解析的落点必须各归各家（本机实测判据）。
    /// 千问办公 auth-v2.dat 在 {APPDATA}\QwenWorkCN（resolve_profile 靠 has_login_data 认它，
    /// 不会漂到 {APPDATA}\QwenWork 这类旧壳残留）；dsh 认 ~/.dsh（.credentials.yaml）；
    /// marvis 就是一条 symlink → 腾讯用户目录，原样返回；xiaomi-mimo 在 {APPDATA}。
    #[test]
    fn new_products_profiles_resolve_to_their_own_dirs() {
        let ad = std::env::var("APPDATA").expect("本机该有 APPDATA");
        let d = profile_dir("qwenwork").expect("千问办公该有档案目录");
        assert_eq!(d, PathBuf::from(&ad).join("QwenWorkCN"), "实际：{d:?}");
        let d = profile_dir("dsh").expect("dsh 该有档案目录");
        assert_eq!(d, home().join(".dsh"), "登录态在 ~/.dsh，不该漂到 APPDATA 那份 userData");
        let d = profile_dir("marvis").expect("marvis 该有档案目录");
        assert_eq!(d, home().join(".marvis"));
        assert!(is_reparse_point(&d), ".marvis 是产品自建 symlink，换号判据走不通（见 apps() 注释）");
        let d = profile_dir("xiaomi-mimo").expect("xiaomi-mimo 该有档案目录");
        assert_eq!(d, PathBuf::from(&ad).join("Xiaomi MiMo"), "实际：{d:?}");
        // 国际版档案解析到 .workbuddy-ai，和国内版 .workbuddy 不撞车
        assert_eq!(profile_dir("workbuddy-intl").as_deref(), Some(home().join(".workbuddy-ai").as_path()));
        assert_eq!(profile_dir("workbuddy").as_deref(), Some(home().join(".workbuddy").as_path()));
    }

    /// WorkBuddy 国内版与国际版的占用者名单互不越界：
    /// 国际版换号绝不能把国内版的 WorkBuddy.exe 列成占用者（反之亦然）。
    #[test]
    fn workbuddy_intl_does_not_claim_domestic_exe_as_locker() {
        let intl = lockers("workbuddy-intl");
        assert!(intl.contains(&"workbuddyai.exe"), "workbuddy-intl 该认自己的 exe：{intl:?}");
        assert!(!intl.contains(&"workbuddy.exe"), "workbuddy-intl 不该把国内版进程算成占用者：{intl:?}");
        let dom = lockers("workbuddy");
        assert!(dom.contains(&"workbuddy.exe"), "workbuddy 该认自己的 exe：{dom:?}");
        assert!(!dom.contains(&"workbuddyai.exe"), "workbuddy 不该把国际版进程算成占用者：{dom:?}");
    }

    /// 新登记的四家（qwenwork/dsh/marvis/xiaomi-mimo）各自只认自家 exe，互不越界；
    /// 也不能把 WorkBuddy/Qoder 系的 exe 混进来。
    #[test]
    fn new_products_lockers_are_self_only() {
        let cases: &[(&str, &[&str])] = &[
            ("qwenwork", &["qwenworkcn.exe"]),
            ("dsh", &["deepseek harness.exe"]),
            ("marvis", &["marvis.exe"]),
            ("xiaomi-mimo", &["xiaomi mimo.exe"]),
        ];
        for (id, must) in cases {
            let l = lockers(id);
            for m in must.iter() {
                assert!(l.contains(m), "{id} 该认自己的 {m}：{l:?}");
            }
            // 不得越界
            for other in ["workbuddy.exe", "workbuddyai.exe", "qoder.exe", "qoder cn.exe"] {
                assert!(!l.contains(&other), "{id} 不该把 {other} 算成占用者：{l:?}");
            }
        }
    }

    /// WorkBuddy 国际版与国内版的档案目录不共用：
    /// 早先 workbuddy-intl 的 profile 整条抄了国内版的 .workbuddy，于是两张卡
    /// 指向同一份档案，自动发现又把真正的 .workbuddy-ai 收成 auto-workbuddyai，
    /// 用户因此看到"两条 WorkBuddy"。这条钉死判据：两者 profile 不同、
    /// 国际版的档案目录就是 .workbuddy-ai（账号用户目录 403fabba-… 确实在它下面）。
    #[test]
    fn workbuddy_intl_and_domestic_have_separate_profiles() {
        let dom = app_def("workbuddy").expect("workbuddy 该在 curated 里");
        let intl = app_def("workbuddy-intl").expect("workbuddy-intl 该在 curated 里");
        assert_eq!(dom.profile.as_deref(), Some(".workbuddy"));
        assert_eq!(intl.profile.as_deref(), Some(".workbuddy-ai"), "国际版该指自己的档案");
        // 两者档案目录归一化后不能相同（不共用）
        let k = |s: &str| s.replace('\\', "/").trim_end_matches('/').to_lowercase();
        assert_ne!(k(dom.profile.as_deref().unwrap()), k(intl.profile.as_deref().unwrap()));
        // 两者 exe 文件名也不能相同（不共用）
        assert_ne!(exe_name(&dom).to_lowercase(), exe_name(&intl).to_lowercase());
        assert_eq!(exe_name(&intl).to_lowercase().as_str(), "workbuddyai.exe");
        // claimed_profile_dirs 必须同时认下两个目录（发现条目不能抢任何一个）
        let claimed = claimed_profile_dirs();
        assert!(claimed.iter().any(|p| p.ends_with("/.workbuddy")), "国内版档案该被认领");
        assert!(claimed.iter().any(|p| p.ends_with("/.workbuddy-ai")), "国际版档案该被认领");
    }
}

pub fn set_swap(cfg: &mut Value, app_id: &str, val: Option<Value>) {
    let obj = cfg
        .as_object_mut()
        .expect("config must be object")
        .entry("swaps")
        .or_insert_with(|| json!({}));
    let o = obj.as_object_mut().expect("swaps must be object");
    match val {
        Some(v) => {
            o.insert(app_id.into(), v);
        }
        None => {
            o.remove(app_id);
        }
    }
}
