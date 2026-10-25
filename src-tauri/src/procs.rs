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
    use sysinfo::{ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
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
    let mut v: Vec<u32> = snap
        .iter()
        .filter(|(_, i)| i.name == exe_name_lower)
        .map(|(p, _)| *p)
        .collect();
    v.sort_unstable();
    v
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
    let mut c = Command::new(exe);
    scrub_env(&mut c);
    c.arg(format!("--user-data-dir={}", data_dir.display()))
        .current_dir(exe.parent().unwrap_or(Path::new(".")));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    c.spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// 直接打开主程序（用默认档案，不隔离）：用于无法多开的程序与 CLI
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
    use super::{poisoned, scrub_env, whole_path_hit, POISON_PREFIX};
    use std::process::Command;

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
}
