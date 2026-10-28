// 进程快照 / 启动 / 结束
use crate::registry::{app_def, exe_name, load_instances, AppDef};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x0000_0008;
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
#[cfg(windows)]
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

#[derive(Clone)]
pub struct ProcInfo {
    pub name: String,
    pub cmdline: String,
}

struct ProcCache {
    at: f64,
    map: HashMap<u32, ProcInfo>,
}

fn cache() -> &'static std::sync::Mutex<ProcCache> {
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<ProcCache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(ProcCache {
            at: 0.0,
            map: HashMap::new(),
        })
    })
}

/// pid -> (映像名小写, 命令行小写)。进程枚举较慢，10 秒内复用缓存。
pub fn snapshot() -> HashMap<u32, ProcInfo> {
    snapshot_inner(false)
}

/// 强制刷新（结束进程等需要实时视图的操作调用）
pub fn snapshot_fresh() -> HashMap<u32, ProcInfo> {
    snapshot_inner(true)
}

fn snapshot_inner(fresh: bool) -> HashMap<u32, ProcInfo> {
    let mut guard = match cache().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let now = now_ts();
    if !fresh && now - guard.at < 10.0 && !guard.map.is_empty() {
        return guard.map.clone();
    }
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut sys = System::new();
    // sysinfo 0.33's default refresh omits command lines. Account ownership
    // depends on the actual arguments; an empty command cannot identify a profile.
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::OnlyIfNotSet));
    let mut out = HashMap::new();
    for (pid, p) in sys.processes() {
        out.insert(
            pid.as_u32(),
            ProcInfo {
                name: p.name().to_string_lossy().to_lowercase(),
                cmdline: p
                    .cmd()
                    .iter()
                    .map(|s| s.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_lowercase(),
            },
        );
    }
    guard.at = now;
    guard.map = out.clone();
    out
}

fn now_ts() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/* ---------------- 有没有"真窗口" ---------------- */
/* Electron 程序常驻后台时进程树还在、窗口一个都没有。只数进程会把这种状态
   报成"运行中"：卡片上只剩「■ 结束」、换号被拒，用户完全不知道自己算不算开着。
   所以另外问一次 user32：哪些 pid 有可见的带标题顶层窗口。 */
#[cfg(windows)]
mod win {
    use std::collections::HashSet;

    #[link(name = "user32")]
    extern "system" {
        fn EnumWindows(cb: unsafe extern "system" fn(isize, isize) -> i32, lparam: isize) -> i32;
        fn IsWindowVisible(hwnd: isize) -> i32;
        fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
        fn GetWindowTextLengthW(hwnd: isize) -> i32;
    }

    unsafe extern "system" fn collect(hwnd: isize, lparam: isize) -> i32 {
        if IsWindowVisible(hwnd) != 0 {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut pid);
            // 只认有标题的：Electron 那一堆隐形辅助窗口没有标题，不算用户"开着"
            if pid != 0 && GetWindowTextLengthW(hwnd) > 0 {
                (*(lparam as *mut HashSet<u32>)).insert(pid);
            }
        }
        1
    }

    pub fn pids_with_window() -> HashSet<u32> {
        let mut set = HashSet::new();
        unsafe {
            EnumWindows(collect, &mut set as *mut HashSet<u32> as isize);
        }
        set
    }
}

#[cfg(not(windows))]
mod win {
    use std::collections::HashSet;
    pub fn pids_with_window() -> HashSet<u32> {
        HashSet::new()
    }
}

/// 有可见窗口的 pid 集合。调用很轻（一次 EnumWindows），但同一轮状态里多个程序
/// 都要问，所以带 10 秒缓存，和进程快照同寿命。
static WIN_CACHE: std::sync::OnceLock<
    std::sync::Mutex<(f64, std::collections::HashSet<u32>)>,
> = std::sync::OnceLock::new();

pub fn windowed_pids() -> std::collections::HashSet<u32> {
    use std::collections::HashSet;
    let c = WIN_CACHE.get_or_init(|| std::sync::Mutex::new((0.0, HashSet::new())));
    let mut g = match c.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let now = now_ts();
    if now - g.0 >= 10.0 {
        g.0 = now;
        g.1 = win::pids_with_window();
    }
    g.1.clone()
}

/// 让进程快照与窗口集合两份缓存立刻过期。
/// 不失效的话，杀完进程后界面还要最长 10 秒才从「■ 结束」翻回「▶ 启动」，
/// 用户以为没生效就会连点。
pub fn invalidate_caches() {
    if let Ok(mut g) = cache().lock() {
        g.at = 0.0;
        g.map.clear();
    }
    if let Some(c) = WIN_CACHE.get() {
        if let Ok(mut g) = c.lock() {
            g.0 = 0.0;
            g.1.clear();
        }
    }
}

/// 这批 pid 里有几个带着可见窗口
pub fn windowed_count(pids: &[u32], vis: &std::collections::HashSet<u32>) -> usize {
    pids.iter().filter(|p| vis.contains(p)).count()
}

/* ---------------- 启动 ---------------- */

/// 从 Agent 的终端里启动驾驶舱时，父进程带着一整套"我是哪个产品"的环境变量
/// （QODER_PRODUCT_ID / QODER_CONFIG_DIR / CODEX_HOME / CLAUDE_CONFIG_DIR…），
/// 子进程原样继承后，被启动的那个程序会以为自己是别的号——实测 Qoder 国际版
/// 因此弹「产物内联的产品是 qoder，当前运行身份是 qoder-cn」并拒绝启动。
/// 一律剥掉，让"从终端启动"和"从开始菜单启动"是同一个干净环境。
const POISON_PREFIX: &[&str] = &["QODER", "CODEX", "CLAUDE", "TRAE", "GEMINI", "ANTIGRAVITY"];

fn poisoned(key: &str) -> bool {
    let k = key.to_uppercase();
    POISON_PREFIX.iter().any(|p| k.starts_with(p))
}

fn scrub_env(c: &mut Command) {
    for (k, _) in std::env::vars_os() {
        if poisoned(&k.to_string_lossy()) {
            c.env_remove(k);
        }
    }
}

/// 给 cliauth 起终端登录用的：内部那份 scrub 不外放，但子进程不能少这一步
pub fn scrub_env_pub(c: &mut Command) {
    scrub_env(c);
}

pub fn alive(snap: &HashMap<u32, ProcInfo>, pid: u32) -> bool {
    snap.contains_key(&pid)
}

/// Electron 实例级进程：命令行里含该账号可能出现的任一路径
pub fn instance_pids(app: &AppDef, cands: &[String], snap: &HashMap<u32, ProcInfo>) -> Vec<u32> {
    let name = exe_name(app).to_lowercase();
    snap.iter()
        .filter(|(_, i)| i.name == name && cands.iter().any(|c| i.cmdline.contains(c)))
        .map(|(p, _)| *p)
        .collect()
}

pub fn program_pids(exe_name_lower: &str, snap: &HashMap<u32, ProcInfo>) -> Vec<u32> {
    // Claude 桌面版与原生 Claude Code CLI 都可能叫 claude.exe。桌面版的
    // 进程表和结束动作必须确认完整 MSIX 路径；读不到身份时不认领同名进程。
    // CLI 实例本来按登记的 PID 管理，不走这份桌面程序名匹配。
    let image = if exe_name_lower == "claude.exe" {
        match app_def("claude-desktop").and_then(|a| crate::registry::resolve_exe(&a)) {
            Some(path) => Some(path),
            None => return Vec::new(),
        }
    } else {
        None
    };
    program_pids_for_image(exe_name_lower, snap, image.as_deref())
}

fn program_pids_for_image(exe_name_lower: &str, snap: &HashMap<u32, ProcInfo>, image: Option<&Path>) -> Vec<u32> {
    let mut v: Vec<u32> = snap
        .iter()
        .filter(|(_, i)| i.name == exe_name_lower
            && image.map(|path| command_starts_with_image(&i.cmdline, path)).unwrap_or(true))
        .map(|(p, _)| *p)
        .collect();
    v.sort_unstable();
    v
}

fn command_starts_with_image(cmdline: &str, image: &Path) -> bool {
    let expected = image.to_string_lossy().replace('\\', "/").to_lowercase();
    if expected.is_empty() {
        return false;
    }
    let command = cmdline.trim_start().replace('\\', "/").to_lowercase();
    if let Some(quoted) = command.strip_prefix('"') {
        return quoted.strip_prefix(&expected)
            .and_then(|tail| tail.strip_prefix('"'))
            .map(|tail| tail.is_empty() || tail.starts_with(char::is_whitespace))
            .unwrap_or(false);
    }
    command.strip_prefix(&expected)
        .map(|tail| tail.is_empty() || tail.starts_with(char::is_whitespace))
        .unwrap_or(false)
}

pub fn kill_pids(pids: &[u32]) {
    // 一个 taskkill 只能带一个 pid，串行 13 个要一秒多；并发跑完只花一次往返
    std::thread::scope(|s| {
        for pid in pids {
            let pid = *pid;
            s.spawn(move || {
                let mut c = Command::new("taskkill");
                c.args(["/pid", &pid.to_string(), "/t", "/f"]);
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    c.creation_flags(CREATE_NO_WINDOW);
                }
                let _ = c.output();
            });
        }
    });
    invalidate_caches();
}

pub fn launch_electron(exe: &Path, data_dir: &Path, workdir: &str) -> Result<(), String> {
    let mut c = Command::new(exe);
    scrub_env(&mut c);
    c.arg(format!("--user-data-dir={}", data_dir.display()))
        .arg(workdir)
        .current_dir(exe.parent().unwrap_or(Path::new(".")));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    c.spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// CLI 系：新开控制台窗口，用环境变量隔离账号，返回 cmd 的 PID（可结束整棵子树）
/// Windows Terminal 的 wt.exe：有它就用它起窗口（可多标签、字体/颜色正常），
/// 没有就退回 conhost(cmd /K)。Windows 11 默认装在 WindowsApps 下。
fn wt_exe() -> Option<PathBuf> {
    // 不引 dirs crate：USERPROFILE 就够，避免为一个路径多一根依赖
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Ok(up) = std::env::var("USERPROFILE") {
        if !up.is_empty() {
            cands.push(
                PathBuf::from(up)
                    .join("AppData")
                    .join("Local")
                    .join("Microsoft")
                    .join("WindowsApps")
                    .join("wt.exe"),
            );
        }
    }
    cands.push(PathBuf::from(r"C:\Program Files\WindowsApps\wt.exe"));
    cands.into_iter().find(|p| p.is_file())
}

pub fn launch_cli(app: &AppDef, exe: &Path, data_dir: &Path, workdir: &str) -> Result<u32, String> {
    // CODEX_HOME 这类隔离变量就是这个账号的目录：多开的每个号一个，互不覆盖
    let setenv = format!("set \"{}={}\" && \"{}\"", app.env, data_dir.display(), exe.display());
    if let Some(wt) = wt_exe() {
        // 标签标题用账号名（data_dir 最后一段），多开时看得出哪个窗口是哪个号
        let acct = data_dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| app.name.clone());
        let mut c = Command::new(&wt);
        scrub_env(&mut c);
        c.args(["--title", &format!("{} · {}", app.name, acct), "-d", workdir, "cmd", "/K", &setenv]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(CREATE_NEW_CONSOLE);
        }
        if let Ok(ch) = c.spawn() {
            return Ok(ch.id());
        }
    }
    let inner = format!("cd /d \"{}\" && {}", workdir, setenv);
    let mut c = Command::new("cmd");
    scrub_env(&mut c);
    c.args(["/K", &inner]).current_dir(workdir);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NEW_CONSOLE);
    }
    c.spawn().map(|ch| ch.id()).map_err(|e| e.to_string())
}

/// Chromium PWA 壳（ChatGPT 桌面版）：只传 --user-data-dir，**不带位置参数**——
/// 它把第一个位置参数当 URL/PWA 入口，传工作目录会开成网页。
/// 不同目录＝不同浏览器实例，实测能与已在跑的实例共存（各自一份登录态）。
pub fn launch_pwa(exe: &Path, data_dir: &Path) -> Result<(), String> {
    launch_pwa_isolated(exe, data_dir, None)
}

pub fn launch_pwa_isolated(exe: &Path, data_dir: &Path, codex_home: Option<&Path>) -> Result<(), String> {
    #[cfg(windows)]
    if exe.to_string_lossy().to_lowercase().contains("\\windowsapps\\") {
        if let Some(home)=codex_home {
            crate::chatgpt::check_plain_path(data_dir)?;crate::chatgpt::check_plain_path(home)?;
            let quote=|p:&Path|format!("'{}'",p.to_string_lossy().replace('\'',"''"));
            let script=format!("{}\nStart-AgentCockpitChatgptPackage -Executable {} -Profile {} -AuthHome {} | Out-Null",include_str!("../scripts/chatgpt-launch.ps1"),quote(exe),quote(data_dir),quote(home));
            let bytes:Vec<u8>=script.encode_utf16().flat_map(u16::to_le_bytes).collect();
            let encoded=crate::secrets::b64_encode(&bytes);
            let mut command=Command::new("powershell.exe");
            use std::os::windows::process::CommandExt;
            let output=command.args(["-NoLogo","-NoProfile","-NonInteractive","-WindowStyle","Hidden","-EncodedCommand",&encoded]).creation_flags(CREATE_NO_WINDOW).output().map_err(|e|e.to_string())?;
            return if output.status.success(){Ok(())}else{Err(format!("商店包启动失败，未回退到默认资料：{}",String::from_utf8_lossy(&output.stderr).chars().take(1000).collect::<String>()))};
        }
    }
    pwa_command(exe, data_dir, codex_home)?.spawn().map(|_| ()).map_err(|e| e.to_string())
}

fn pwa_command(exe: &Path, data_dir: &Path, codex_home: Option<&Path>) -> Result<Command, String> {
    crate::chatgpt::check_plain_path(data_dir)?;
    let mut c = Command::new(exe);
    scrub_env(&mut c);
    if let Some(home) = codex_home {
        crate::chatgpt::check_plain_path(home)?;
        for key in ["OPENAI_API_KEY", "OPENAI_BASE_URL", "OPENAI_ACCESS_TOKEN", "OPENAI_IDENTITY_TOKEN_FILE"] { c.env_remove(key); }
        c.env("CODEX_HOME", home).env("CODEX_ELECTRON_USER_DATA_PATH", data_dir);
    }
    c.arg(format!("--user-data-dir={}", data_dir.display()))
        .current_dir(exe.parent().unwrap_or(Path::new(".")));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    Ok(c)
}

#[cfg(test)]
mod chatgpt_launch_tests {
    use super::*;
    #[test]
    fn managed_instances_have_separate_browser_and_auth_paths() {
        let root=crate::registry::home().join("chatgpt-launch-test");
        for name in ["account-a","account-b"] {
            let browser=root.join(name).join("browser");let home=root.join(name).join("auth");
            let c=pwa_command(&root.join("ChatGPT.exe"),&browser,Some(&home)).unwrap();
            let env:std::collections::HashMap<_,_>=c.get_envs().map(|(k,v)|(k.to_string_lossy().into_owned(),v.map(|v|v.to_string_lossy().into_owned()))).collect();
            assert_eq!(env["CODEX_HOME"].as_deref(),Some(home.to_string_lossy().as_ref()));
            assert_eq!(env["CODEX_ELECTRON_USER_DATA_PATH"].as_deref(),Some(browser.to_string_lossy().as_ref()));
            assert!(env["OPENAI_API_KEY"].is_none());assert!(env["OPENAI_BASE_URL"].is_none());
            assert_eq!(c.get_args().next().unwrap().to_string_lossy(),format!("--user-data-dir={}",browser.display()));
        }
    }
}

/// 打开本机默认客户端。Claude 桌面版必须走注册包入口，不能借用 CLI
/// 的 CLAUDE_CONFIG_DIR，也不能把 Claude-3p 改成独立实例目录。
pub fn launch_default_app(app: &AppDef, exe: &Path) -> Result<u32, String> {
    if crate::registry::registered_app_id(&app.id).is_some() {
        return launch_registered_app(&app.id);
    }
    if app.kind == "cli" {
        launch_terminal(exe)
    } else {
        launch_plain(exe)
    }
}

#[cfg(windows)]
fn registered_app_command(app_id: &str) -> Result<Command, String> {
    use std::os::windows::process::CommandExt;
    let aumid = crate::registry::registered_app_id(app_id)
        .ok_or_else(|| "该程序没有注册包启动入口".to_string())?;
    let windows = std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into());
    let mut command = Command::new(PathBuf::from(windows).join("explorer.exe"));
    scrub_env(&mut command);
    command.arg(format!("shell:AppsFolder\\{aumid}"));
    command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    Ok(command)
}

fn launch_registered_app(app_id: &str) -> Result<u32, String> {
    #[cfg(windows)]
    {
        // Explorer 通过 AppsFolder 激活注册应用；这里返回的是启动器 PID，
        // 调用方只报告打开动作，不把它登记成 Claude 实例进程。
        return registered_app_command(app_id)?
            .spawn().map(|child| child.id()).map_err(|e| e.to_string());
    }
    #[cfg(not(windows))]
    {
        let _ = app_id;
        Err("该桌面客户端需要 Windows 注册包入口".to_string())
    }
}

/// 打开登记的只读资料位置。固定产品路径，不收前端 path，也不创建目录。
pub fn open_native_data_dir(app_id: &str) -> Result<(), String> {
    let path = crate::registry::native_data_dir(app_id)
        .ok_or_else(|| "该程序没有登记只读资料目录".to_string())?;
    if !path.is_dir() {
        return Err("Claude 资料目录不存在，请先在官方客户端完成初始化".to_string());
    }
    let windows = std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into());
    let mut command = Command::new(PathBuf::from(windows).join("explorer.exe"));
    scrub_env(&mut command);
    command.arg(path);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(all(test, windows))]
mod registered_app_launch_tests {
    use super::*;

    #[test]
    fn claude_desktop_uses_registered_package_entry_without_profile_arguments() {
        let command = registered_app_command("claude-desktop").unwrap();
        assert_eq!(Path::new(command.get_program()).file_name().unwrap(), "explorer.exe");
        let args: Vec<_> = command.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, ["shell:AppsFolder\\Claude_pzs8sxrjxfjjc!Claude"]);
        assert!(command.get_current_dir().is_none(), "不要借用安装目录或资料目录作为工作目录");
        assert!(!command.get_envs().any(|(k, v)| v.is_some() && poisoned(&k.to_string_lossy())));
        assert!(registered_app_command("claude").is_err(), "不得用桌面包入口启动 CLI");
        assert!(registered_app_command("codex-desktop").is_err(), "不改变现有 ChatGPT 多开入口");
    }
}

/// 直接打开主程序（用默认档案，不隔离）：用于普通原生程序
pub fn launch_plain(exe: &Path) -> Result<u32, String> {
    let mut c = Command::new(exe);
    scrub_env(&mut c);
    if let Some(dir) = exe.parent() {
        c.current_dir(dir);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    c.spawn().map(|ch| ch.id()).map_err(|e| e.to_string())
}

/// 新开一个终端跑 CLI 程序（不注入隔离变量）
pub fn launch_terminal(exe: &Path) -> Result<u32, String> {
    let mut c = Command::new("cmd");
    scrub_env(&mut c);
    c.args(["/K", &format!("\"{}\"", exe.display())]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NEW_CONSOLE);
    }
    c.spawn().map(|ch| ch.id()).map_err(|e| e.to_string())
}

/// 换号/清缓存前的占用检查：该 Agent 是否有任何窗口在跑
pub fn agent_running(app_id: &str, snap: &HashMap<u32, ProcInfo>) -> bool {
    let app = match app_def(app_id) {
        Some(a) => a,
        None => return false,
    };
    if app.kind == "electron" || app.kind == "plain" || app.kind == "pwa" {
        !program_pids(&exe_name(&app).to_lowercase(), snap).is_empty()
    } else {
        load_instances(app_id).iter().any(|m| {
            let pid = m["pid"].as_u64().unwrap_or(0) as u32;
            pid != 0 && alive(snap, pid)
        })
    }
}

pub fn locker_pids(app_id: &str, snap: &HashMap<u32, ProcInfo>) -> Vec<(u32, String)> {
    let wanted = crate::registry::lockers(app_id);
    snap.iter()
        .filter(|(_, i)| wanted.iter().any(|w| *w == i.name))
        .map(|(p, i)| (*p, i.name.clone()))
        .collect()
}

/// 谁的命令行里出现了这个目录——rename 被拒时靠它指出真正的占用者。
/// 只按 exe 名匹配会漏（Qoder 国际版/Qoder CN 是两个 exe，还会带一队渲染子进程，
/// 有时是 node/agent 进程在读写档案目录）。proc 快照的 cmdline 已统一小写。
pub fn pids_touching(dir: &Path, snap: &HashMap<u32, ProcInfo>) -> Vec<(u32, String)> {
    let needle = dir
        .to_string_lossy()
        .to_lowercase()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string();
    if needle.trim_matches('/').is_empty() {
        return Vec::new();   // 空路径会命中所有进程，必须挡住
    }
    let mut v: Vec<(u32, String)> = snap
        .iter()
        .filter(|(_, i)| whole_path_hit(&i.cmdline.replace('\\', "/"), &needle))
        .map(|(p, i)| (*p, i.name.clone()))
        .collect();
    v.sort_unstable();
    v
}

/// 路径要**整段**命中才算占用：`...\Roaming\Qoder` 不能因为 `...\Roaming\QoderCN\...`
/// 里含了它的前缀就把隔壁产品的进程算成占用者
fn whole_path_hit(hay: &str, needle: &str) -> bool {
    hay.match_indices(needle).any(|(idx, m)| {
        match hay[idx + m.len()..].chars().next() {
            None => true,
            Some(c) => matches!(c, '/' | '"' | '\'' | ' ' | ';' | ','),
        }
    })
}

pub fn procs_rows(snap: &HashMap<u32, ProcInfo>) -> Vec<Value> {
    let mut rows = Vec::new();
    for app in crate::registry::all_apps() {
        if crate::registry::resolve_exe(&app).is_none() {
            continue;
        }
        if app.kind == "electron" || app.kind == "plain" || app.kind == "pwa" {
            let pids = program_pids(&exe_name(&app).to_lowercase(), snap);
            if !pids.is_empty() {
                let wins = windowed_count(&pids, &windowed_pids());
                rows.push(json!({
                    "app": app.name, "app_id": app.id,
                    "detail": if wins > 0 {
                        format!("{} 个窗口 · {} 个进程（含渲染/GPU 子进程）", wins, pids.len())
                    } else {
                        format!("{} 个进程，一个窗口都没有（后台驻留）", pids.len())
                    },
                    "windows": wins,
                    "pids": pids.iter().take(6).map(|p| p.to_string()).collect::<Vec<_>>().join(","),
                    "count": pids.len(),
                }));
            }
        } else {
            for m in load_instances(&app.id) {
                let pid = m["pid"].as_u64().unwrap_or(0) as u32;
                if pid != 0 && alive(snap, pid) {
                    rows.push(json!({
                        "app": format!("{} / {}", app.name, m["name"].as_str().unwrap_or("")),
                        "app_id": app.id,
                        "detail": m["workdir"].as_str().unwrap_or(""),
                        "pids": pid.to_string(),
                        "count": 1,
                    }));
                }
            }
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::{command_starts_with_image, poisoned, program_pids_for_image, scrub_env, whole_path_hit, ProcInfo, POISON_PREFIX};
    use std::collections::HashMap;
    use std::path::Path;
    use std::process::Command;

    #[test]
    fn claude_desktop_processes_exclude_same_named_cli_and_unproven_identity() {
        let image = Path::new(r"C:\Program Files\WindowsApps\Claude_3.6556.2.0_x64__pzs8sxrjxfjjc\app\Claude.exe");
        let desktop = image.to_string_lossy();
        let entries = [
            (1, desktop.to_string()),
            (2, format!("\"{desktop}\" --type=renderer")),
            (3, format!("{} --type=gpu-process", desktop.replace('\\', "/").to_uppercase())),
            (4, r"C:\Users\x\.local\bin\claude.exe --resume".to_string()),
            (5, format!("C:\\Tools\\claude.exe --inspect \"{desktop}\"")),
            (6, format!("{desktop}.old --type=renderer")),
            (7, String::new()),
            (8, "claude.exe".to_string()),
        ];
        let snap: HashMap<_, _> = entries.into_iter().map(|(pid, cmdline)| {
            (pid, ProcInfo { name: "claude.exe".into(), cmdline })
        }).collect();
        assert_eq!(program_pids_for_image("claude.exe", &snap, Some(image)), [1, 2, 3]);
        // 路径收紧只对需要确认映像的调用生效，其它产品仍保留原来的名称判据。
        assert_eq!(program_pids_for_image("claude.exe", &snap, None).len(), snap.len());
    }

    #[test]
    fn claude_desktop_image_match_requires_complete_leading_executable() {
        let image = Path::new("C:/Program Files/WindowsApps/Claude_1_x64__pzs8sxrjxfjjc/app/Claude.exe");
        let path = image.to_string_lossy();
        assert!(command_starts_with_image(&format!("  \"{path}\"\t--type=utility"), image));
        assert!(!command_starts_with_image(&format!("C:/CLI/claude.exe --path={path}"), image));
        assert!(!command_starts_with_image(&format!("\"{path} backup\""), image));
        assert!(!command_starts_with_image(&format!("\"{path}\".old"), image));
        assert!(!command_starts_with_image("anything", Path::new("")));
    }

    /// 路径要整段命中：`...\Roaming\Qoder` 不能把 `...\Roaming\QoderCN\...` 的进程
    /// 算成占用者，否则给国际版换号时会把隔壁 CN 的进程全列进阻塞名单
    #[test]
    fn path_match_needs_a_boundary() {
        let needle = "c:/users/x/appdata/roaming/qoder";
        assert!(whole_path_hit(r#"--user-data-dir="c:/users/x/appdata/roaming/qoder""#, needle));
        assert!(whole_path_hit("c:/users/x/appdata/roaming/qoder/logs/a.log", needle));
        assert!(whole_path_hit("x c:/users/x/appdata/roaming/qoder y", needle));
        assert!(whole_path_hit(needle, needle), "整条命令行就是它本身");
        assert!(!whole_path_hit("c:/users/x/appdata/roaming/qodercn/state.vscdb", needle));
        assert!(!whole_path_hit("c:/users/x/appdata/roaming/qoder-backup/x", needle));
    }

    /// 真的起一个 cmd 把环境打出来：驾驶舱启动子进程时环境里必须已经没有 QODER_*/CODEX_* 这些身份变量
    #[test]
    fn scrubbed_child_env_has_no_agent_identity() {
        let mut c = Command::new("cmd");
        scrub_env(&mut c);
        c.args(["/d", "/c", "set"]);
        let out = c.output().expect("起不了 cmd");
        let s = String::from_utf8_lossy(&out.stdout).to_lowercase();
        for p in POISON_PREFIX {
            let needle = format!("\n{}=", p.to_lowercase());
            assert!(!s.contains(&needle), "子进程环境里还漏着 {}* 变量", p);
        }
        assert!(s.contains("\npath="), "PATH 不该被清掉");
    }

    /// 从 Agent 的终端里启动驾驶舱时会带着一整套"我是哪个产品"的变量，必须全清掉，
    /// 否则被启动的程序会以为自己是别的号（实测 Qoder 国际版弹自检错误拒绝启动）
    #[test]
    fn agent_identity_vars_get_scrubbed() {
        for k in [
            "QODER_PRODUCT_ID",
            "QODER_CONFIG_DIR",
            "QODERCN_CONFIG_DIR",
            "QODERCN_CLI",
            "QODERCLI_RUNTIME_PACKAGING",
            "QODER_WORKER_CWD",
            "CODEX_HOME",
            "CLAUDE_CONFIG_DIR",
            "codex_home",
            "TRAE_HOME",
        ] {
            assert!(poisoned(k), "{k} 该剥掉");
        }
        for k in [
            "PATH",
            "APPDATA",
            "LOCALAPPDATA",
            "USERPROFILE",
            "COMSPEC",
            "SYSTEMROOT",
            "TEMP",
            "OS",
        ] {
            assert!(!poisoned(k), "{k} 不该动");
        }
    }

    #[cfg(windows)]
    #[test]
    fn snapshot_reads_the_current_process_arguments() {
        let processes=super::snapshot_fresh();
        let current=processes.get(&std::process::id()).expect("current process is present");
        assert!(!current.cmdline.trim().is_empty(), "account ownership requires command-line collection");
    }
}
