// 换号模式：把 Agent 默认档案目录做成指向实例目录的 NTFS junction（零额外空间）
use crate::procs::{agent_running, kill_pids, snapshot_fresh};
use crate::registry::{
    find_instance, load_config, profile_dir, save_config, set_swap, swaps_of,
};
use std::fs;
use std::path::Path;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// junction / symlink 检测：交给 registry 的同一份实现，免得判据分叉
pub fn is_junction(p: &Path) -> bool {
    crate::registry::is_reparse_point(p)
}

/// 删掉链接本身，绝不碰它指向的内容。
/// 目标已经不存在的断链，Rust 的 remove_dir 有时会报 path-not-found，退回 cmd 的 rmdir
/// （rmdir 不带 /S 时只摘链接）——断链恰恰是最需要摘掉的那种，不能在这儿失败。
/// 换号、迁移数据根都走这一份实现，摘链接的判据不许分叉。
pub(crate) fn remove_link(link: &Path) -> bool {
    if fs::remove_dir(link).is_ok() {
        return !is_junction(link);
    }
    let mut c = std::process::Command::new("cmd");
    c.args(["/C", "rmdir", &link.to_string_lossy()]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = c.output();
    !crate::registry::is_reparse_point(link)
}

/// 默认档案入口成了断链（指着已经不存在的账号目录）时把链接摘掉，返回是否修了。
/// 留着它，目标程序连"自己新建一份档案"的机会都没有，只会硬报无法写入数据目录。
pub fn repair_dangling(app_id: &str) -> bool {
    if app_id == crate::zcode::APP { return false; }
    if app_id == "codex-desktop" { return false; }
    let p = match crate::registry::profile_dir(app_id) {
        Some(p) => p,
        None => return false,
    };
    if !is_junction(&p) || p.exists() {
        return false;
    }
    remove_link(&p)
}

static REPORT: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

pub fn last_report() -> Vec<String> {
    REPORT.lock().map(|g| g.clone()).unwrap_or_default()
}

/// 启动时把「换号记录」和「磁盘上的 junction」对一遍，以磁盘为准修正，顺手修掉断链。
/// 两者会脱节（账号改过名、换号中途失败、手工删过目录），而脱节的后果不是显示错，
/// 是目标程序**直接起不来**：默认入口指着已经不存在的账号目录，开始菜单图标也会变白纸。
/// 只做不需要程序停着的动作（摘链接 / 建链接 / 改记录），不搬任何数据。
pub fn self_check() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cfg = load_config();
    let mut changed = false;
    for app in crate::registry::all_apps_raw() {
        let id = app.id.as_str();
        // Native ChatGPT profiles are never repaired by creating/removing links.
        if id == "codex-desktop" || id == crate::zcode::APP {
            if profile_dir(id).is_some_and(|p| is_junction(&p)) {
                out.push(format!("{}资料目录仍有旧链接，请先备份并单独处理；本次启动未改动资料。",app.name));
            }
            continue;
        }
        let p = match profile_dir(id) {
            Some(p) => p,
            None => continue,
        };
        let rec = swaps_of(&cfg).get(id).cloned();
        // 共用一份档案的产品（Qoder 系 / WorkBuddy / ZCode / TRAE…）：账号目录就是默认档案
        // 本身，压根没有"换号"这回事。记录里若还留着一条，只可能是早期版本或一次失败的换号
        // 留下的残渣——照下面那套按磁盘对账，只会得出"记录说 A、磁盘说 B"这种谁也看不懂的
        // 告警（Qoder CN 上就是这么冒出来的），而磁盘本来就是对的。清掉即可，不用报。
        if crate::registry::shares_one_profile(id) {
            if swaps_of(&cfg).get(id).is_some() {
                set_swap(&mut cfg, id, None);
                changed = true;
            }
            continue;
        }
        let rec_name = rec
            .as_ref()
            .and_then(|c| c["instance"].as_str())
            .unwrap_or("")
            .to_string();
        let original = rec
            .as_ref()
            .and_then(|c| c["original"].as_str())
            .unwrap_or("")
            .to_string();
        let bound_name = crate::registry::bound_instance_name(id);

        if is_junction(&p) && crate::registry::reparse_bound_dir(&p).is_none() {
            let target = if rec_name.is_empty() {
                None
            } else {
                find_instance(id, &rec_name)
                    .and_then(|m| m["data_dir"].as_str().map(std::path::PathBuf::from))
                    .filter(|d| d.is_dir())
            };
            let _ = remove_link(&p);
            let repaired = match &target {
                Some(d) => mklink_junction(&p, d).is_ok() && is_junction(&p),
                None => false,
            };
            changed = true;
            if repaired {
                out.push(format!(
                    "{} 的默认档案入口本来是指向已删除账号的断链，已重新接回「{}」",
                    app.name, rec_name
                ));
            } else {
                set_swap(&mut cfg, id, None);
                out.push(format!(
                    "{} 的默认档案入口是个指向已删除账号的断链，已摘掉；下次打开会新建一份干净档案（账号目录里的数据没动）",
                    app.name
                ));
            }
            continue;
        }

        // 记录"是不是原地账号"这一栏也得跟磁盘对得上：只有名字对得上、形状不对，
        // 「还原默认」照样会走错分支
        let disk_in_place = bound_name.is_some() && crate::registry::bound_instance_dir(id).is_none();
        let rec_in_place = rec
            .as_ref()
            .and_then(|c| c["in_place"].as_bool())
            .unwrap_or(false);
        if rec_name != bound_name.clone().unwrap_or_default() || disk_in_place != rec_in_place {
            changed = true;
            // 记录里没存退路（早期 adopt 就是空串），可磁盘上真躺着 .cockpit-original 的话
            // 必须把它补回记录——否则「还原默认」会把那份原档案晾着、还回一个空目录
            let original = if original.is_empty() {
                crate::registry::aside_backup_of(&p)
            } else {
                original
            };
            match &bound_name {
                Some(n) => {
                    // 磁盘上没有 junction 却认得出当前账号 = 原地账号。
                    // 这里必须写 in_place 而不是 original:""，否则「还原默认」会照着
                    // 空 original 判成"接管账号没有退路"，还回一个空档案。
                    let rec = if crate::registry::bound_instance_dir(id).is_none() {
                        serde_json::json!({ "instance": n, "in_place": true })
                    } else {
                        serde_json::json!({ "instance": n, "original": original })
                    };
                    set_swap(&mut cfg, id, Some(rec));
                    out.push(format!(
                        "{} 的换号记录和磁盘对不上（记录写的是「{}」，实际挂着的是「{}」），已按磁盘改成「{}」",
                        app.name,
                        if rec_name.is_empty() { "没换号" } else { rec_name.as_str() },
                        n,
                        n
                    ));
                }
                None => {
                    set_swap(&mut cfg, id, None);
                    out.push(format!(
                        "{} 的换号记录说当前账号是「{}」，但默认档案目录已经不是换号链接了（可能手工还原过），已清掉这条记录",
                        app.name, rec_name
                    ));
                }
            }
        }
    }
    if changed {
        save_config(&cfg);
    }
    // 累加而不是覆盖：启动体检之后几毫秒还会跟着自动扫描再跑一轮，那一轮是干净的，
    // 直接覆盖会把刚修好的东西报不出来，界面上一条都看不见。
    if let Ok(mut g) = REPORT.lock() {
        for m in &out {
            if !g.contains(m) {
                g.push(m.clone());
            }
        }
    }
    out
}

pub(crate) fn mklink_junction(link: &Path, target: &Path) -> Result<String, String> {
    let mut c = std::process::Command::new("cmd");
    c.args([
        "/C",
        "mklink",
        "/J",
        &link.to_string_lossy(),
        &target.to_string_lossy(),
    ]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    let out = c.output().map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// 换号前找"谁在占着这个档案目录"：两条线索合起来——
/// ① 已知会持有句柄的进程名（registry::lockers）
/// ② 命令行里出现这个目录路径的进程（Electron 渲染子进程、node/agent 进程常带 --user-data-dir）
/// 只看 ① 会漏：Qoder 国际版与 Qoder CN 是两个 exe，agent 进程还可能整个不认识自己那份目录。
fn blockers_of(
    app: &crate::registry::AppDef,
    app_id: &str,
    dir: &Path,
    snap: &std::collections::HashMap<u32, crate::procs::ProcInfo>,
) -> Vec<(u32, String)> {
    let mut out: Vec<(u32, String)> = crate::procs::program_pids(
        &crate::registry::exe_name(app).to_lowercase(),
        snap,
    )
    .into_iter()
    .map(|p| (p, crate::registry::exe_name(app).to_lowercase()))
    .collect();
    for (pid, name) in crate::procs::locker_pids(app_id, snap)
        .into_iter()
        .chain(crate::procs::pids_touching(dir, snap))
    {
        if !out.iter().any(|(p, _)| *p == pid) {
            out.push((pid, name));
        }
    }
    out.sort_unstable();
    out
}

fn blocker_names(list: &[(u32, String)]) -> String {
    let mut names: Vec<String> = list
        .iter()
        .map(|(p, n)| format!("{}（PID {}）", n, p))
        .collect();
    names.sort();
    names.dedup();
    names.join("、")
}

/// 换号的结果。`need_force` 是结构化信号：界面据此决定摆不摆「结束进程并换号」，
/// 不再拿中文文案去正则匹配（曾经文案改了个词，那个按钮就再也出不来，
/// 而提示还写着"点「强制换号」"——用户对着一个不存在的按钮干着急）。
#[derive(Debug, Clone)]
pub struct SwapOut {
    pub ok: bool,
    pub msg: String,
    /// 这个账号的档案就是程序默认档案本身，切过去在磁盘上什么都不改（共用一份档案的产品）
    pub noop: bool,
    /// 被在跑的进程挡住了，而结束它们确实能解开
    pub need_force: bool,
}

impl SwapOut {
    fn ok(msg: impl Into<String>) -> Self {
        SwapOut { ok: true, msg: msg.into(), noop: false, need_force: false }
    }
    fn err(msg: impl Into<String>) -> Self {
        SwapOut { ok: false, msg: msg.into(), noop: false, need_force: false }
    }
}

pub fn swap_set(app_id: &str, name: &str, force: bool) -> SwapOut {
    if app_id == crate::zcode::APP { return SwapOut::err("ZCode 使用凭据切换，不执行目录交换"); }
    if app_id == "codex-desktop" { return SwapOut::err("ChatGPT使用独立实例登录，已禁止目录链接换号。默认资料未改动。"); }
    let app = match crate::registry::app_def(app_id) {
        Some(a) => a,
        None => return SwapOut::err("未知 Agent"),
    };
    let p = match profile_dir(app_id) {
        Some(p) => p,
        None => return SwapOut::err("该 Agent 未登记默认档案目录，不支持换号"),
    };
    // 运行时家目录守卫：Qoder 系这类目录（.auth / ipc / host-actions / projects / mcp-router.json）
    // 是 Agent 自己的家，不是"能换号的登录档案"。换它既换不动登录，又必然被在跑的 agent 占住；
    // 万一以后 products 表又被写错，这里拦住，别去 rename 一个正在使用的运行时目录。
    if crate::registry::looks_like_runtime_home(&p) {
        return SwapOut::err(format!(
            "{} 是 Agent 运行时家目录（.auth / ipc / host-actions / projects 都在里面），不是能换号的账号档案。\n\
             换号要换登录态所在的那个目录：Qoder 0.4.x 是 %APPDATA%\\com.qoder.app.stable（auth.v1.dat），\n\
             旧壳才是 %APPDATA%\\Qoder。这里不动它，免得把正在跑的 Agent 弄坏。",
            p.display()
        ));
    }
    let meta = match find_instance(app_id, name) {
        Some(m) => m,
        None => return SwapOut::err("实例不存在"),
    };
    let instance_dir = match meta["data_dir"].as_str() {
        Some(d) => std::path::PathBuf::from(d),
        None => return SwapOut::err("实例缺少 data_dir"),
    };
    // 共用一份档案的产品（单窗口换号：Qoder 系、WorkBuddy、ZCode、TRAE…）：这个账号的档案
    // 就是默认档案那一份，切过去磁盘上一个字节都不改。必须挡在占用检查**前面**——
    // 她只是换个号，没有任何理由因此被要求关掉程序。
    if crate::registry::swap_changes_nothing(app_id, &instance_dir) {
        return SwapOut {
            ok: true,
            noop: true,
            need_force: false,
            msg: format!(
                "{} 的所有账号共用这一份档案（{}），切到「{}」在磁盘上没有东西要换。\n\
                 真正的切换在客户端里做：退出登录，再登「{}」。\n\
                 不用关掉程序，驾驶舱也不会去动你的进程；登进来之后这张卡会自己认出当前登录的是它。",
                app.name, p.display(), name, name
            ),
        };
    }
    let snap = snapshot_fresh();
    if app.kind == "cli" {
        if agent_running(app_id, &snap) {
            return SwapOut::err(format!("请先关闭所有 {} 窗口/终端再换号", app.name));
        }
    }
    let blockers = blockers_of(&app, app_id, &p, &snap);
    if !blockers.is_empty() && !force {
        let wins = crate::procs::windowed_count(
            &blockers.iter().map(|(p, _)| *p).collect::<Vec<_>>(),
            &crate::procs::windowed_pids(),
        );
        return SwapOut {
            need_force: true,
            ..SwapOut::err(format!(
                "{} 现在被这些进程占着：{}。\n{}",
                p.display(),
                blocker_names(&blockers),
                if wins > 0 {
                    "换号要把这个目录改名，得先关掉它们的窗口。确认后驾驶舱可以替你结束这些进程再换号。"
                } else {
                    "它们没有窗口但在后台驻留（换号、改名都会被挡住）。确认后驾驶舱可以替你结束这些进程再换号。"
                }
            ))
        };
    }
    if !blockers.is_empty() {
        kill_pids(&blockers.iter().map(|(p, _)| *p).collect::<Vec<_>>());
        std::thread::sleep(std::time::Duration::from_millis(900));
    }

    let mut cfg = load_config();
    let cur = swaps_of(&cfg).get(app_id).cloned();
    // 「已经是当前账号」得让磁盘认账，不能只看记录：记录说已经是、链接却指着别处
    // 或者已经断了，这里直接返回就永远修不好——Qoder 国际版那次断链就是这么留下的。
    if cur.as_ref().and_then(|c| c["instance"].as_str()) == Some(name)
        && crate::registry::is_bound_to(app_id, &instance_dir)
    {
        return SwapOut::ok("该实例已是当前账号");
    }
    if is_junction(&p) {
        let _ = remove_link(&p); // 只删链接，不碰目标
    }
    // 原地账号：它的数据本来就在默认位置上，换号只是把当初改名让位的那份换回来，
    // 全程同一父目录内改名，不挂链接、不跨目录搬任何东西。
    if crate::registry::is_in_place(app_id, &instance_dir) {
        return swap_to_in_place(app_id, name, &mut cfg, &p);
    }
    let mut original: Option<std::path::PathBuf> = None;
    if p.exists() {
        let orig_path = format!("{}.cockpit-original", p.display());
        let orig = Path::new(&orig_path);
        if orig.exists() {
            return SwapOut::err("已存在 .cockpit-original 备份目录，请先「还原默认」再换号");
        }
        if let Err(e) = fs::rename(&p, orig) {
            // 走到这里说明上面那一轮没能清干净（或者占用者根本不是本产品的进程）。
            // 别把 os error 5 直接丢给用户——重新抓一次实时快照，把还占着的进程点名出来。
            let fresh = snapshot_fresh();
            let left = blockers_of(&app, app_id, &p, &fresh);
            return SwapOut::err(if left.is_empty() {
                format!(
                    "换不动 {}：Windows 说「拒绝访问」（{}）。可能有别的程序（杀毒/同步盘/资源管理器）正停在这个目录上，\
                     或者有进程把它当工作目录。关掉那个窗口再试一次。",
                    p.display(),
                    e
                )
            } else {
                format!(
                    "换不动 {}：还被这些进程占着——{}。\n它们可能刚被其它程序重新拉起，\
                     手动结束它们（任务管理器按名字搜）后再点「设为当前」。",
                    p.display(),
                    blocker_names(&left)
                )
            });
        }
        original = Some(orig.to_path_buf());
    }
    // 磁盘上本来没有可搬走的档案（例如上次换号只留了个断链），但旧记录里那份
    // .cockpit-original 还在——继续记着它，否则「还原默认」就找不回原档案了。
    if original.is_none() {
        if let Some(o) = cur.as_ref().and_then(|c| c["original"].as_str()) {
            if !o.is_empty() && Path::new(o).exists() {
                original = Some(std::path::PathBuf::from(o));
            }
        }
    }

    if let Err(e) = mklink_junction(&p, &instance_dir) {
        if let Some(orig) = &original {
            let _ = fs::rename(orig, &p);
        }
        return SwapOut::err(format!("创建 junction 失败：{}", e));
    }
    if !is_junction(&p) {
        if let Some(orig) = &original {
            let _ = fs::remove_dir(&p);
            let _ = fs::rename(orig, &p);
        }
        return SwapOut::err("创建 junction 失败：链接未生效");
    }

    set_swap(
        &mut cfg,
        app_id,
        Some(serde_json::json!({
            "instance": name,
            "original": original.map(|o| o.to_string_lossy().into_owned()).unwrap_or_default(),
        })),
    );
    save_config(&cfg);
    // 切到空账号目录会让目标程序显示未登录/空白界面，必须当场说清楚怎么退回
    let note = if crate::identity::inspect(&instance_dir.to_string_lossy()).state == "none" {
        "\n注意：这个账号目录还是空的，程序打开会是未登录或空白界面（个别程序如 WorkBuddy 会显示黑屏）。\
         你的原档案已备份，随时点「还原默认」即可恢复。"
    } else {
        ""
    };
    SwapOut::ok(format!(
        "已换号：{} 默认入口 → 「{}」，零额外空间{}",
        app.name, name, note
    ))
}

/// 切到一个「原地账号」：链接上面已经摘掉了，这里把让位时改名的那份换回默认位置。
/// 记录里写 `in_place: true`，`swap_clear` 与界面据此知道"这个账号本来就是默认档案"。
fn swap_to_in_place(
    app_id: &str,
    name: &str,
    cfg: &mut serde_json::Value,
    p: &Path,
) -> SwapOut {
    if p.is_dir() {
        // 默认位置已经就是这个账号，只是记录没跟上
    } else {
        let aside = crate::registry::aside_backup_of(p);
        let orig = Path::new(&aside);
        if aside.is_empty() || !orig.exists() {
            return SwapOut::err(format!(
                "换不过去：{} 空着，且同级没有 .cockpit-original 可以换回来。\n\
                 这个账号的数据可能已经被手工挪动过，先确认它现在在哪。",
                p.display()
            ));
        }
        if let Err(e) = fs::rename(orig, p) {
            return SwapOut::err(format!("把「{}」换回默认位置失败：{e}", name));
        }
    }
    set_swap(
        cfg,
        app_id,
        Some(serde_json::json!({ "instance": name, "in_place": true })),
    );
    save_config(cfg);
    SwapOut::ok(format!(
        "已换号：「{}」的档案本来就在 {}，没有搬家。",
        name,
        p.display()
    ))
}

pub fn swap_clear(app_id: &str) -> (bool, String) {
    if app_id == crate::zcode::APP { return (false, "ZCode 数据保持原位，不执行目录交换".into()); }
    if app_id == "codex-desktop" { return (false, "ChatGPT默认资料受保护，不执行目录交换。".into()); }
    let app = match crate::registry::app_def(app_id) {
        Some(a) => a,
        None => return (false, "未知 Agent".into()),
    };
    let p = match profile_dir(app_id) {
        Some(p) => p,
        None => return (false, "该 Agent 未登记默认档案目录".into()),
    };
    let mut cfg = load_config();
    let cur = swaps_of(&cfg).get(app_id).cloned();
    let cur = match cur {
        Some(c) => c,
        None => return (true, "当前未处于换号状态".into()),
    };
    // 原地账号本来就是这台机器的默认档案：默认位置没有链接、也没有让位备份，
    // 磁盘上没有任何东西可还原。记录保留（否则启动体检会照着磁盘把它重新写回来）。
    // 这一条必须走在"程序在跑"之前：没有文件要动，就没有资格要求用户关掉客户端。
    if cur["in_place"].as_bool().unwrap_or(false) && !is_junction(&p) {
        return (
            true,
            format!(
                "「{}」本来就是 {} 的默认档案，数据一直在 {}，没有搬家，也没有可还原的备份。",
                cur["instance"].as_str().unwrap_or(""),
                app.name,
                p.display()
            ),
        );
    }
    if agent_running(app_id, &snapshot_fresh()) {
        return (false, format!("请先关闭所有 {} 窗口/终端再还原", app.name));
    }
    // 原地账号的换号记录若真挂着链接（下面要改名），才需要程序停着
    if is_junction(&p) {
        let _ = remove_link(&p);
    }
    let orig = cur["original"].as_str().unwrap_or("");
    let adopted = orig.is_empty();
    if !adopted && Path::new(orig).exists() {
        if let Err(e) = fs::rename(orig, &p) {
            return (false, format!("还原原档案失败：{}", e));
        }
    }
    set_swap(&mut cfg, app_id, None);
    save_config(&cfg);
    if adopted {
        // 这个账号当初是「接管」现有档案建起来的，默认档案没有可还原的备份
        (
            true,
            format!(
                "已解除 {} 的换号。它原来的数据就是这个账号目录本身，不会丢；\
                 从默认入口再打开会是一个全新的档案。",
                app.name
            ),
        )
    } else {
        (true, format!("已还原 {} 默认档案", app.name))
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 把早期 adopt 搬进 `instances/` 的档案**搬回它本来的位置**，账号改判为原地账号。
/// 同盘 rename 是元数据操作，不复制内容；每一步失败都按原样退回，绝不留下断链。
/// 原生位置被别的东西占着就拒绝动手。
pub fn rehome(app_id: &str, name: &str, force: bool) -> (bool, String) {
    if app_id == crate::zcode::APP { return (false, "ZCode 数据保持原位，不执行目录搬移".into()); }
    if app_id == "codex-desktop" { return (false, "ChatGPT资料不再自动搬移；原有资料保留。".into()); }
    let app = match crate::registry::app_def(app_id) {
        Some(a) => a,
        None => return (false, "未知 Agent".into()),
    };
    let meta = match find_instance(app_id, name) {
        Some(m) => m,
        None => return (false, "实例不存在".into()),
    };
    let data_dir = match meta["data_dir"].as_str() {
        Some(d) => std::path::PathBuf::from(d),
        None => return (false, "实例缺少 data_dir".into()),
    };
    // 判据只有一份：registry::rehome_slot 认定它是"搬进 instances/ 的接管账号"才许归位
    let native = match crate::registry::rehome_slot(app_id, &meta) {
        Some(p) => p,
        None => {
            return (
                false,
                format!(
                    "「{}」不是搬进 instances/ 的接管账号，没有可归位的原位置。\n\
                     如果这个程序的默认档案此刻正让位在 .cockpit-original 里，那该点的是「还原默认」，不是归位。",
                    name
                ),
            )
        }
    };
    if !data_dir.is_dir() {
        return (false, format!("账号目录 {} 不存在，没什么可归位", data_dir.display()));
    }

    let snap = snapshot_fresh();
    let mut blockers = blockers_of(&app, app_id, &native, &snap);
    for (pid, pname) in crate::procs::pids_touching(&data_dir, &snap) {
        if !blockers.iter().any(|(p, _)| *p == pid) {
            blockers.push((pid, pname));
        }
    }
    if !blockers.is_empty() && !force {
        return (
            false,
            format!(
                "{} 还在跑：{}。\n归位要把它的档案目录改名，得先退出程序。点「强制归位」驾驶舱来结束它们。",
                app.name,
                blocker_names(&blockers)
            ),
        );
    }
    if !blockers.is_empty() {
        kill_pids(&blockers.iter().map(|(p, _)| *p).collect::<Vec<_>>());
        std::thread::sleep(std::time::Duration::from_millis(900));
    }

    let bytes = crate::space::dir_size(&data_dir);
    let mb = crate::space::human_mb(bytes);

    // 原生位置上还留着让位备份（.cockpit-original）的话先挪开：
    // 它既挡着归位，又会让以后每次换号都报"已存在备份目录"。
    let aside = std::path::PathBuf::from(crate::registry::aside_backup_of(&native));
    let mut aside_note = String::new();
    if aside.is_dir() {
        let keep = crate::registry::base_dir()
            .join("backups")
            .join(format!("rehome-{}", now_secs()))
            .join(app_id);
        let dest = keep.join(leaf(&aside));
        let mut moved = fs::create_dir_all(&keep).is_ok() && fs::rename(&aside, &dest).is_ok();
        if moved {
            aside_note = format!("\n原生位置的让位备份已收进 {}。", dest.display());
        } else {
            // 跨盘或不许建目录：就地改名留底，至少同盘 rename 一定成
            let local = std::path::PathBuf::from(format!("{}.rehome-{}", aside.display(), now_secs()));
            moved = fs::rename(&aside, &local).is_ok();
            if moved {
                aside_note = format!("\n原生位置的让位备份就地留底为 {}。", local.display());
            } else {
                return (
                    false,
                    format!("{} 挡在原生位置上，既挪不进 backups 也没能就地改名留底，先手工处理它。", aside.display()),
                );
            }
        }
    }

    let was_link = is_junction(&native);
    if was_link && !remove_link(&native) {
        return (false, format!("摘掉 {} 上的换号链接失败，一个字节都没动", native.display()));
    }
    let rollback = |why: String| -> (bool, String) {
        if native.is_dir() && !is_junction(&native) {
            let _ = fs::rename(&native, &data_dir);
        }
        if was_link {
            let _ = mklink_junction(&native, &data_dir);
        }
        (false, format!("{}（换号链接已按原样接回，数据没动）", why))
    };
    if native.exists() {
        return rollback(format!("{} 上还有别的东西，不敢覆盖", native.display()));
    }
    if let Some(parent) = native.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return rollback(format!("建 {} 的父目录失败：{e}", native.display()));
        }
    }
    if let Err(e) = fs::rename(&data_dir, &native) {
        return rollback(format!("归位失败：{e}"));
    }
    if is_junction(&native) || !native.is_dir() {
        return rollback("归位后校验失败：默认位置不是这个目录本身".to_string());
    }
    let after = crate::space::dir_size(&native);
    if after != bytes {
        return rollback(format!(
            "归位后体积对不上（{} MB → {} MB），先别打开这个程序",
            mb,
            crate::space::human_mb(after)
        ));
    }

    let mut m = meta.clone();
    m["data_dir"] = serde_json::json!(native.to_string_lossy());
    m["rehomed"] = serde_json::json!(now_secs());
    crate::registry::save_instance(app_id, &m);
    let mut cfg = load_config();
    let hit = swaps_of(&cfg)
        .get(app_id)
        .and_then(|c| c["instance"].as_str())
        == Some(name);
    if hit {
        set_swap(
            &mut cfg,
            app_id,
            Some(serde_json::json!({ "instance": name, "in_place": true })),
        );
        save_config(&cfg);
    }
    if fs::read_dir(&data_dir).map(|mut r| r.next().is_none()).unwrap_or(false) {
        let _ = fs::remove_dir(&data_dir);
    }
    (
        true,
        format!(
            "已归位：「{}」的档案从 {} 搬回 {}（{} MB，同盘改名，没有复制）。{}这个账号现在是原地账号，数据不再离开默认位置。",
            name,
            meta["data_dir"].as_str().unwrap_or(""),
            native.display(),
            mb,
            aside_note
        ),
    )
}

fn leaf(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "aside".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机才跑（`cargo test -- --include-ignored`）。手动往 config 里塞一条 Qoder CN 的换号记录，
    /// 再跑一遍启动体检：**共用一份档案的产品不该有换号记录可以对账**——照旧对账只会得出
    /// 「记录说 A、磁盘说 B」这种谁也看不懂的告警（她 10-15 截图里那条红框），而磁盘本来就是对的。
    /// 记录该被静默清掉，且不许报出任何带产品名的句子。
    /// 跑完把 config 原样还回去，失败也还（Drop 兜底）。
    #[test]
    #[ignore]
    fn real_machine_shared_profile_records_are_dropped_without_a_warning() {
        if !crate::registry::shares_one_profile("qoder-cn") {
            println!("这台机器的 Qoder CN 不是共用档案形态，跳过");
            return;
        }
        let before = crate::registry::load_config();
        struct Restore(serde_json::Value);
        impl Drop for Restore {
            fn drop(&mut self) {
                crate::registry::save_config(&self.0);
            }
        }
        let _guard = Restore(before.clone());

        let mut cfg = crate::registry::load_config();
        crate::registry::set_swap(
            &mut cfg,
            "qoder-cn",
            Some(serde_json::json!({ "instance": "nick9994374911" })),
        );
        crate::registry::save_config(&cfg);
        assert!(crate::registry::swaps_of(&crate::registry::load_config())
            .get("qoder-cn")
            .is_some());

        let out = self_check();
        for m in &out {
            assert!(
                !m.contains("Qoder CN"),
                "共用一份档案的产品不该冒出换号对账告警：{m}"
            );
        }
        assert!(
            crate::registry::swaps_of(&crate::registry::load_config())
                .get("qoder-cn")
                .is_none(),
            "残留的换号记录该被清掉"
        );
    }
}
