// 数据根目录治理：驾驶舱的数据不该挤在 C 盘。
//
// 三层解析（resolve_root）：环境变量 AGENT_COCKPIT_HOME > 家目录下的指针文件
// `.agent-cockpit-root`（一行绝对路径）> 默认 `~/.agent-cockpit`。
// 指针文件只有几十字节，真身整体搬到别的盘，C 盘不再替别的产品背十几 GB。
//
// 迁移＝复制→逐条目校验→改写新副本里的绝对路径→切指针→重挂入口链接→旧真身改名留底。
// 失败时旧真身一字未动，成功也留着留底，界面有「退回原来的位置」。
use crate::registry::{self as reg, home};
use crate::swap;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub const DIR_NAME: &str = ".agent-cockpit";
pub const POINTER_NAME: &str = ".agent-cockpit-root";
pub const JOURNAL_NAME: &str = ".agent-cockpit-migration.json";
/// 迁移成功后旧真身改名的后缀（留着当保险，体检里能一键删）
pub const KEEPASIDE_SUFFIX: &str = ".pre-migration";
/// 首次落位时给非系统盘起的文件夹名
pub const DATA_PARENT: &str = "AgentData";
/// 自动选盘的最低空闲量（GB）
const MIN_FREE_GB: u64 = 20;

// ---------------------------------------------------------------- 盘符信息

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn GetDiskFreeSpaceExW(
        lp_directory_name: *const u16,
        lp_free_bytes_available_to_caller: *mut u64,
        lp_total_number_of_bytes: *mut u64,
        lp_total_number_of_free_bytes: *mut u64,
    ) -> i32;
    fn GetVolumeInformationW(
        lp_root_path_name: *const u16,
        lp_volume_name_buffer: *mut u16,
        n_volume_name_size: u32,
        lp_volume_serial_number: *mut u32,
        lp_maximum_component_length: *mut u32,
        lp_file_system_flags: *mut u32,
        lp_file_system_name_buffer: *mut u16,
        n_file_system_name_size: u32,
    ) -> i32;
    fn GetDriveTypeW(lp_root_path_name: *const u16) -> u32;
}

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn volume_row(root: &str, letter: &str) -> Value {
    #[cfg(windows)]
    {
        let w = wide(root);
        let mut free_user = 0u64;
        let mut total = 0u64;
        let mut free_all = 0u64;
        let ok =
            unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut free_user, &mut total, &mut free_all) };
        let mut fs_name = [0u16; 32];
        let mut serial = 0u32;
        let mut flags = 0u32;
        let got = unsafe {
            GetVolumeInformationW(
                w.as_ptr(),
                std::ptr::null_mut(),
                0,
                &mut serial,
                std::ptr::null_mut(),
                &mut flags,
                fs_name.as_mut_ptr(),
                fs_name.len() as u32,
            )
        };
        let fs = if got != 0 {
            String::from_utf16_lossy(&fs_name)
                .trim_end_matches('\0')
                .to_string()
        } else {
            String::new()
        };
        let fixed = ok != 0 && unsafe { GetDriveTypeW(w.as_ptr()) } == 3;
        json!({
            "drive": letter,
            "root": root,
            "fixed": fixed,
            "fs": fs,
            "total_gb": gb(total),
            "free_gb": gb(free_user),
            "ntfs": fs.eq_ignore_ascii_case("NTFS") || fs.eq_ignore_ascii_case("ReFS"),
        })
    }
    #[cfg(not(windows))]
    {
        json!({ "drive": letter, "root": root, "fixed": false, "fs": "",
                "total_gb": 0, "free_gb": 0, "ntfs": false })
    }
}

/// 路径里的盘符；UNC 或相对路径返回 None
pub fn drive_of(p: &Path) -> Option<String> {
    let s = p.to_string_lossy().replace('/', "\\");
    let c: Vec<char> = s.chars().collect();
    if c.len() >= 2 && c[1] == ':' && c[0].is_ascii_alphabetic() {
        Some(c[0].to_ascii_uppercase().to_string())
    } else {
        None
    }
}

pub fn system_drive() -> String {
    drive_of(&home()).unwrap_or_else(|| "C".into())
}

fn free_gb_of(p: &Path) -> u64 {
    match drive_of(p) {
        Some(d) => volume_row(&format!("{}:\\", d), &d)["free_gb"]
            .as_u64()
            .unwrap_or(0),
        None => 0,
    }
}

fn gb(bytes: u64) -> u64 {
    bytes / 1024 / 1024 / 1024
}

/// 本地固定磁盘，非系统盘排前面（"优先别的盘"就是界面看到的顺序）
pub fn drives() -> Vec<Value> {
    let mut out: Vec<Value> = (b'B'..=b'Z')
        .map(|b| {
            let letter = (b as char).to_string();
            volume_row(&format!("{}:\\", letter), &letter)
        })
        .filter(|v| v["fixed"].as_bool().unwrap_or(false))
        .collect();
    let sys = system_drive();
    out.sort_by_key(|v| {
        if v["drive"].as_str().unwrap_or("") == sys {
            1
        } else {
            0
        }
    });
    out
}

// ---------------------------------------------------------------- 根目录解析

pub fn default_root() -> PathBuf {
    home().join(DIR_NAME)
}

pub fn pointer_file() -> PathBuf {
    home().join(POINTER_NAME)
}

pub fn journal_file() -> PathBuf {
    home().join(JOURNAL_NAME)
}

fn read_pointer() -> Option<PathBuf> {
    let s = fs::read_to_string(pointer_file()).ok()?;
    let s = s.trim().replace('/', "\\");
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

/// 指针指向的盘还在不在。移动硬盘拔掉、或换台机器打开，都要能退回默认位置，
/// 绝不能因为根目录读不到就启动失败。
fn volume_present(p: &Path) -> bool {
    match drive_of(p) {
        Some(d) => Path::new(&format!("{}:\\", d)).exists(),
        None => true,
    }
}

pub fn resolve_root() -> PathBuf {
    if let Some(v) = std::env::var_os("AGENT_COCKPIT_HOME") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    match read_pointer() {
        Some(p) if volume_present(&p) => p,
        _ => default_root(),
    }
}

pub fn root_is_default() -> bool {
    key(&resolve_root()) == key(&default_root())
}

/// 测试用的共享层重定向（和 keys.rs 的 store_override 同一套路）：
/// 不设就是 None，走真实数据根，测试绝不许写进用户的 ~/.agent-cockpit/shared
#[cfg(test)]
static SHARED_OVERRIDE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

#[cfg(test)]
pub fn set_shared_override(dir: Option<PathBuf>) {
    *SHARED_OVERRIDE.lock().unwrap_or_else(|p| p.into_inner()) = dir;
}

pub fn shared_dir(app_id: &str) -> PathBuf {
    #[cfg(test)]
    {
        if let Some(p) = SHARED_OVERRIDE.lock().unwrap_or_else(|p| p.into_inner()).clone() {
            return p.join(app_id);
        }
    }
    resolve_root().join("shared").join(app_id)
}

/// 全新机器（还没安家）时按盘况决定落位，别让第一份数据就压在系统盘上。
/// 已经有数据在默认位置时**一律不动**——搬家是显式动作，不是启动时的副作用。
pub fn ensure_first_run() {
    if pointer_file().exists() || default_root().exists() {
        return;
    }
    if let Some(v) = std::env::var_os("AGENT_COCKPIT_HOME") {
        if !v.is_empty() {
            return;
        }
    }
    let Some(p) = first_run_suggestion().map(|(p, _)| p) else {
        return;
    };
    // 建得成才写指针；目标盘只读、被占用等情况退回默认位置，别让程序起不来
    if fs::create_dir_all(p.join("instances")).is_ok() && fs::create_dir_all(p.join("backups")).is_ok()
    {
        let _ = fs::write(pointer_file(), format!("{}\n", p.to_string_lossy()));
    }
}

/// 这台机器还没有任何驾驶舱数据时，挑个划算的落位：非系统 NTFS 盘里空闲最大的。
/// 返回 None 就老老实实安家目录。
pub fn first_run_suggestion() -> Option<(PathBuf, String)> {
    let best = drives()
        .into_iter()
        .filter(|d| {
            d["ntfs"].as_bool().unwrap_or(false)
                && d["drive"].as_str().unwrap_or("") != system_drive()
                && d["free_gb"].as_u64().unwrap_or(0) >= MIN_FREE_GB
        })
        .max_by_key(|d| d["free_gb"].as_u64().unwrap_or(0))?;
    let letter = best["drive"].as_str().unwrap_or("D");
    Some((
        PathBuf::from(format!("{}:\\{}\\{}", letter, DATA_PARENT, DIR_NAME)),
        format!(
            "{} 盘是 NTFS、还剩 {} GB，比 {} 盘宽裕",
            letter,
            best["free_gb"].as_u64().unwrap_or(0),
            system_drive()
        ),
    ))
}

/// Windows 路径比较：大小写与 / \ 的差异都算同一个路径
pub fn key(p: &Path) -> String {
    p.to_string_lossy().replace('/', "\\").to_lowercase()
}

/// 字符串如果落在 `old` 底下，换成 `new` 底下的等价路径；不是则返回 None
fn remap_str(s: &str, old: &str, new: &str) -> Option<String> {
    let norm: String = s.chars().map(|c| if c == '/' { '\\' } else { c }).collect();
    let nk = norm.to_lowercase();
    let ok = old.to_lowercase();
    if !nk.starts_with(&ok) {
        return None;
    }
    let n = old.chars().count();
    // 整段命中才算：`.agent-cockpit-backup` 不是 `.agent-cockpit` 的子目录
    if nk.chars().count() > n && nk.chars().nth(n) != Some('\\') {
        return None;
    }
    let rest: String = norm.chars().skip(n).collect();
    Some(format!("{}{}", new, rest))
}

fn remap_value(v: &mut Value, old: &str, new: &str) -> usize {
    match v {
        Value::String(s) => match remap_str(s, old, new) {
            Some(t) => {
                *s = t;
                1
            }
            None => 0,
        },
        Value::Array(a) => a.iter_mut().map(|x| remap_value(x, old, new)).sum(),
        Value::Object(m) => m.iter_mut().map(|(_, x)| remap_value(x, old, new)).sum(),
        _ => 0,
    }
}

// ---------------------------------------------------------------- 占用统计

/// 目录里每个条目的 (相对路径, 字节)——迁移校验与去重比对都靠它，目录本身记 0
pub fn fingerprint(p: &Path) -> Vec<(String, u64)> {
    let mut out: Vec<(String, u64)> = Vec::new();
    walk_into(p, p, 0, &mut out);
    out.sort();
    out
}

fn walk_into(base: &Path, cur: &Path, depth: u32, out: &mut Vec<(String, u64)>) {
    if depth > 24 {
        return;
    }
    let rd = match fs::read_dir(cur) {
        Ok(r) => r,
        Err(_) => return,
    };
    for e in rd.filter_map(|x| x.ok()) {
        let path = e.path();
        let md = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let rel = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if reg::is_reparse_point(&path) {
            out.push((format!("{}/", rel), 0));
        } else if md.is_dir() {
            out.push((format!("{}/", rel), 0));
            walk_into(base, &path, depth + 1, out);
        } else {
            out.push((rel, md.len()));
        }
    }
}

pub fn dir_bytes(p: &Path) -> u64 {
    fingerprint(p).iter().map(|(_, b)| *b).sum()
}

/// 每个账号目录的真实占用（链接层不穿透，只认真身）
fn instances_report() -> Vec<Value> {
    let dir = resolve_root().join("instances");
    let mut out = Vec::new();
    let rd = match fs::read_dir(&dir) {
        Ok(r) => r,
        Err(_) => return out,
    };
    for app in rd.filter_map(|x| x.ok()).filter(|x| x.path().is_dir()) {
        let app_id = app.file_name().to_string_lossy().into_owned();
        let mut accounts = Vec::new();
        let mut app_total = 0u64;
        let sub = match fs::read_dir(app.path()) {
            Ok(r) => r.filter_map(|x| x.ok()).collect::<Vec<_>>(),
            Err(_) => Vec::new(),
        };
        for acc in sub {
            let p = acc.path();
            if !p.is_dir() || reg::is_reparse_point(&p) {
                continue;
            }
            let bytes = dir_bytes(&p);
            app_total += bytes;
            accounts.push(json!({
                "name": acc.file_name().to_string_lossy(),
                "bytes": bytes,
                "mb": bytes / 1024 / 1024,
            }));
        }
        out.push(json!({
            "app": app_id,
            "bytes": app_total,
            "mb": app_total / 1024 / 1024,
            "accounts": accounts,
        }));
    }
    out.sort_by(|a, b| {
        b["bytes"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["bytes"].as_u64().unwrap_or(0))
    });
    out
}

/// 家目录 / APPDATA / LOCALAPPDATA 顶层那些指进 `root` 的 junction。
/// 迁移时得跟着改目标，否则 WorkBuddy 它们当场找不到自己的档案。
pub fn inbound_links_for(root: &Path) -> Vec<PathBuf> {
    let rk = key(root);
    let mut out = Vec::new();
    for base in reg::profile_search_dirs() {
        let rd = match fs::read_dir(&base) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for e in rd.filter_map(|x| x.ok()) {
            let p = e.path();
            if !reg::is_reparse_point(&p) {
                continue;
            }
            if let Ok(t) = fs::read_link(&p) {
                if key(&reg::strip_verbatim(&t)).starts_with(&rk) {
                    out.push(p);
                }
            }
        }
    }
    out
}

pub fn inbound_links() -> Vec<PathBuf> {
    inbound_links_for(&resolve_root())
}

/// 所有入口链接及各自当前目标，用来看有没有断链
fn all_links() -> Vec<(PathBuf, PathBuf)> {
    let mut out = Vec::new();
    for base in reg::profile_search_dirs() {
        let rd = match fs::read_dir(&base) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for e in rd.filter_map(|x| x.ok()) {
            let p = e.path();
            if !reg::is_reparse_point(&p) {
                continue;
            }
            if let Ok(t) = fs::read_link(&p) {
                out.push((p, reg::strip_verbatim(&t)));
            }
        }
    }
    out
}

// ---------------------------------------------------------------- 垃圾体检

/// 扫出来的可回收项先登记在这，删除只认登记过的 id——界面传进来的不是路径
static JUNK: std::sync::Mutex<Vec<(String, PathBuf, u64, String)>> =
    std::sync::Mutex::new(Vec::new());

pub fn junk_note(kind: &str) -> &'static str {
    match kind {
        "keepaside" => "接管原有档案前的留底。确认现在这个账号能正常打开、对话都在，就可以删。",
        "restoring" => "「从备份恢复」中途失败的残骸，账号没用它，删掉不影响任何账号。",
        "partial" => "虚拟机组件断点下载的过期分片。程序真要用的时候会自己重下。",
        "backup" => "账号备份 zip。删掉只丢这个时间点的快照，账号本身不受影响。",
        "stray" => "backups 里不是 zip 的杂项（多半是手工修数据时顺手丢进去的）。",
        "junkfile" => "配置的历史备份 / 测试污染文件，程序不读它们。",
        "oldroot" => "迁到别的盘之后留在原位置的真身留底。新位置跑通过才可以删。",
        _ => "",
    }
}

fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

fn push_junk(out: &mut Vec<Value>, p: &Path, kind: &str, bytes: u64) {
    out.push(json!({
        "id": format!("{:x}", fnv(&p.to_string_lossy())),
        "kind": kind,
        "path": p.to_string_lossy(),
        "bytes": bytes,
        "mb": bytes / 1024 / 1024,
        "note": junk_note(kind),
    }));
}

/// 限定深度的递归：找名字符合的目录，不整份遍历真身
fn find_dirs(base: &Path, depth: u32, max: u32, hit: &mut Vec<PathBuf>, pred: &dyn Fn(&str) -> bool) {
    if depth > max {
        return;
    }
    let rd = match fs::read_dir(base) {
        Ok(r) => r,
        Err(_) => return,
    };
    for e in rd.filter_map(|x| x.ok()) {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        let is_dir = p.is_dir() && !reg::is_reparse_point(&p);
        if !is_dir {
            continue;
        }
        if pred(&name) {
            hit.push(p.clone());
        }
        find_dirs(&p, depth + 1, max, hit, pred);
    }
}

pub fn junk_scan() -> Vec<Value> {
    let root = resolve_root();
    let mut out: Vec<Value> = Vec::new();

    // 1) 接管留底与迁移留底
    if let Ok(rd) = fs::read_dir(home()) {
        for e in rd.filter_map(|x| x.ok()) {
            let name = e.file_name().to_string_lossy().into_owned();
            let p = e.path();
            if p.is_dir() && name.ends_with(".cockpit-original") {
                push_junk(&mut out, &p, "keepaside", dir_bytes(&p));
            } else if p.is_dir() && name.ends_with(KEEPASIDE_SUFFIX) {
                push_junk(&mut out, &p, "oldroot", dir_bytes(&p));
            }
        }
    }

    // 2) 恢复残骸 + 3) 断点下载分片（共享层里也可能有，两边都要看）
    let inst = root.join("instances");
    let shared = root.join("shared");
    let mut restoring = Vec::new();
    for base in [inst.clone(), shared.clone()] {
        find_dirs(&base, 0, 2, &mut restoring, &|n| n.ends_with(".restoring"));
    }
    for p in restoring {
        push_junk(&mut out, &p, "restoring", dir_bytes(&p));
    }
    let mut dl = Vec::new();
    for base in [inst, shared] {
        find_dirs(&base, 0, 8, &mut dl, &|n| {
            n == ".aha-net-downloads" || n.ends_with("-downloads")
        });
    }
    for dir in dl {
        if let Ok(rd) = fs::read_dir(&dir) {
            for e in rd.filter_map(|x| x.ok()) {
                let p = e.path();
                if p.is_dir() && e.file_name().to_string_lossy().starts_with("attempt-") {
                    push_junk(&mut out, &p, "partial", dir_bytes(&p));
                }
            }
        }
    }

    // 4) 两周以上的备份与杂物
    let cutoff = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() - 14 * 86400)
        .unwrap_or(0);
    let bak = root.join("backups");
    if let Ok(rd) = fs::read_dir(&bak) {
        for e in rd.filter_map(|x| x.ok()) {
            let p = e.path();
            let md = match e.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let old = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() < cutoff)
                .unwrap_or(false);
            if !old {
                continue;
            }
            if p.is_file() && p.extension().map(|x| x == "zip").unwrap_or(false) {
                push_junk(&mut out, &p, "backup", md.len());
            } else if p.is_dir() {
                push_junk(&mut out, &p, "stray", dir_bytes(&p));
            }
        }
    }

    // 5) 根目录里的历史备份 / 测试污染文件
    if let Ok(rd) = fs::read_dir(&root) {
        for e in rd.filter_map(|x| x.ok()) {
            let name = e.file_name().to_string_lossy().into_owned();
            let p = e.path();
            let is_junk = name.contains(".bak-")
                || name.ends_with(".test-pollution.bak")
                || (name.ends_with(".bak") && p.is_file());
            if is_junk && p.is_file() {
                push_junk(
                    &mut out,
                    &p,
                    "junkfile",
                    fs::metadata(&p).map(|m| m.len()).unwrap_or(0),
                );
            }
        }
    }

    out.sort_by(|a, b| {
        b["bytes"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["bytes"].as_u64().unwrap_or(0))
    });
    if let Ok(mut g) = JUNK.lock() {
        *g = out
            .iter()
            .map(|v| {
                (
                    v["id"].as_str().unwrap_or("").to_string(),
                    PathBuf::from(v["path"].as_str().unwrap_or("")),
                    v["bytes"].as_u64().unwrap_or(0),
                    v["kind"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect();
    }
    out
}

pub fn junk_clean(ids: &[String]) -> (bool, String) {
    let allow: Vec<(String, PathBuf, u64, String)> =
        JUNK.lock().map(|g| g.clone()).unwrap_or_default();
    let mut freed = 0u64;
    let mut done = 0usize;
    let mut errs: Vec<String> = Vec::new();
    for id in ids {
        let item = match allow.iter().find(|(i, ..)| i == id) {
            Some(x) => x,
            None => {
                errs.push(format!("{} 不在本次扫描结果里，没动", id));
                continue;
            }
        };
        let (p, bytes, kind) = (item.1.clone(), item.2, item.3.clone());
        // 迁移留底要新位置真跑起来过才让删
        if kind == "oldroot" {
            let st = read_journal().and_then(|j| j["status"].as_str().map(String::from));
            if st.as_deref() != Some("done") {
                errs.push(format!("{}：迁移还没确认成功，先别删留底", p.display()));
                continue;
            }
        }
        let is_link = reg::is_reparse_point(&p);
        let r = if is_link {
            fs::remove_dir(&p).or_else(|_| fs::remove_file(&p))
        } else if p.is_dir() {
            fs::remove_dir_all(&p)
        } else {
            fs::remove_file(&p)
        };
        match r {
            Ok(_) => {
                freed += bytes;
                done += 1;
            }
            Err(e) => errs.push(format!("{} 删不掉：{}", p.display(), e)),
        }
    }
    (
        done > 0,
        format!(
            "已清理 {} 项，回收 {} MB{}",
            done,
            freed / 1024 / 1024,
            if errs.is_empty() {
                String::new()
            } else {
                format!("；失败：{}", errs.join("；"))
            }
        ),
    )
}

// ---------------------------------------------------------------- 后台统计

static COUNTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static CACHED: std::sync::Mutex<Option<Value>> = std::sync::Mutex::new(None);

/// 逐目录统计要走几万个文件（WorkBuddy 一家 13 万），放在 IPC 线程上会把整个窗口
/// 冻住几十秒。所以丢给后台线程，先回 counting，界面隔几秒来取结果。
pub fn sizes_report(refresh: bool) -> Value {
    if refresh {
        if let Ok(mut g) = CACHED.lock() {
            *g = None;
        }
    }
    if let Some(v) = CACHED.lock().ok().and_then(|mut g| g.take()) {
        COUNTING.store(false, std::sync::atomic::Ordering::SeqCst);
        return v;
    }
    if !COUNTING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        std::thread::spawn(|| {
            let done = json!({
                "ok": true,
                "report": report(true),
                "junk": junk_scan(),
            });
            if let Ok(mut g) = CACHED.lock() {
                *g = Some(done);
            }
        });
    }
    json!({ "ok": true, "counting": true })
}

/// 界面报表

pub fn keepaside_path() -> Option<PathBuf> {
    let d = default_root();
    let p = d.with_file_name(format!(
        "{}{}",
        d.file_name()?.to_string_lossy(),
        KEEPASIDE_SUFFIX
    ));
    p.exists().then_some(p)
}

pub fn report(with_sizes: bool) -> Value {
    let root = resolve_root();
    let mut instances = Vec::new();
    let mut shared_bytes = 0u64;
    let mut backup_bytes = 0u64;
    let mut total_bytes = 0u64;
    if with_sizes {
        instances = instances_report();
        shared_bytes = dir_bytes(&root.join("shared"));
        backup_bytes = dir_bytes(&root.join("backups"));
        total_bytes = dir_bytes(&root);
    }
    let mut by_app: Vec<(String, u64)> = instances
        .iter()
        .map(|v| {
            (
                v["app"].as_str().unwrap_or("").to_string(),
                v["bytes"].as_u64().unwrap_or(0),
            )
        })
        .collect();
    by_app.sort_by(|a, b| b.1.cmp(&a.1));
    json!({
        "root": root.to_string_lossy(),
        "drive": drive_of(&root).unwrap_or_default(),
        "default_root": default_root().to_string_lossy(),
        "is_default": root_is_default(),
        "pointer": pointer_file().to_string_lossy(),
        "has_pointer": pointer_file().exists(),
        "free_gb": free_gb_of(&root),
        "sys_free_gb": free_gb_of(&home()),
        "sys_drive": system_drive(),
        "drives": drives(),
        "suggested": first_run_suggestion().map(|(p, why)| json!({
            "path": p.to_string_lossy(), "why": why,
        })),
        "inbound": inbound_links()
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        "dangling": all_links()
            .iter()
            .filter(|(_, t)| !t.exists())
            .map(|(l, t)| json!({ "link": l.to_string_lossy(), "target": t.to_string_lossy() }))
            .collect::<Vec<_>>(),
        "instances": instances,
        "shared_mb": shared_bytes / 1024 / 1024,
        "backups_mb": backup_bytes / 1024 / 1024,
        "total_mb": total_bytes / 1024 / 1024,
        "sizes": with_sizes,
        "journal": read_journal(),
        "keepaside": keepaside_path().map(|p| p.to_string_lossy().into_owned()),
    })
}

// ---------------------------------------------------------------- 迁移

pub fn read_journal() -> Option<Value> {
    fs::read_to_string(journal_file())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
}

fn write_journal(j: &Value) {
    let _ = fs::write(
        journal_file(),
        serde_json::to_string_pretty(j).unwrap_or_default(),
    );
}

/// 每一步都记进 journal：卡在哪、动了什么，界面照着念，用户不用猜
fn step(j: &mut Value, name: &str, detail: &str) {
    let line = format!(
        "{} {}",
        chrono::Local::now().format("%H:%M:%S"),
        if detail.is_empty() {
            name.to_string()
        } else {
            format!("{}：{}", name, detail)
        }
    );
    if let Some(a) = j["steps"].as_array_mut() {
        a.push(json!(line));
    }
    write_journal(j);
}

fn target_check(target: &Path) -> Result<(), String> {
    let s = target.to_string_lossy().replace('/', "\\");
    if drive_of(Path::new(&s)).is_none() {
        return Err("请填绝对路径，比如 D:\\AgentData\\.agent-cockpit".into());
    }
    let d = drive_of(Path::new(&s)).unwrap_or_default();
    let row = volume_row(&format!("{}:\\", d), &d);
    if !row["fixed"].as_bool().unwrap_or(false) {
        return Err(format!("{}: 不是本地固定磁盘", d));
    }
    if !row["ntfs"].as_bool().unwrap_or(false) {
        return Err(format!(
            "{}: 是 {}，装不了账号数据（换号靠 NTFS junction）",
            d,
            row["fs"].as_str().unwrap_or("?")
        ));
    }
    let old = resolve_root();
    if key(target) == key(&old) {
        return Err("目标就是现在的位置，不用迁".into());
    }
    let (tk, ok2) = (key(target), key(&old));
    if tk.starts_with(&ok2) || ok2.starts_with(&tk) {
        return Err("新旧位置互相嵌套，会把对方当成自己的子目录".into());
    }
    if target.exists() {
        let has = fs::read_dir(target)
            .map(|mut r| r.next().is_some())
            .unwrap_or(false);
        if has {
            return Err(format!("目标 {} 里已经有东西，换个空目录", target.display()));
        }
    }
    Ok(())
}

/// 复制真身：链接按链接搬（不穿透、不跟着链接把别处数据整份拖进来）
fn copy_preserving(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| format!("建目录失败：{}", e))?;
    let rd = fs::read_dir(src).map_err(|e| format!("读不了 {}：{}", src.display(), e))?;
    for e in rd.filter_map(|x| x.ok()) {
        let from = e.path();
        let to = dst.join(e.file_name());
        let md = fs::symlink_metadata(&from).map_err(|e| format!("{}", e))?;
        if reg::is_reparse_point(&from) {
            let t = fs::read_link(&from).map_err(|e| format!("读链接失败：{}", e))?;
            swap::mklink_junction(&to, &reg::strip_verbatim(&t))
                .map_err(|e| format!("重建链接失败：{}", e))?;
        } else if md.is_dir() {
            copy_preserving(&from, &to)?;
        } else if md.is_file() {
            fs::copy(&from, &to).map_err(|e| format!("复制 {} 失败：{}", from.display(), e))?;
        }
    }
    Ok(())
}

fn rewrite_json_at(p: &Path, old: &str, new: &str) -> usize {
    match fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    {
        Some(mut v) => {
            let hit = remap_value(&mut v, old, new);
            if hit > 0
                && fs::write(p, serde_json::to_string_pretty(&v).unwrap_or_default()).is_ok()
            {
                hit
            } else {
                0
            }
        }
        None => 0,
    }
}

fn rewrite_json_under(dir: &Path, old: &str, new: &str) -> usize {
    let mut n = 0usize;
    let rd = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    for e in rd.filter_map(|x| x.ok()) {
        let p = e.path();
        if p.is_dir() {
            n += rewrite_json_under(&p, old, new);
        } else if p.extension().map(|x| x == "json").unwrap_or(false) {
            n += rewrite_json_at(&p, old, new);
        }
    }
    n
}

/// 把指进 `from` 的入口链接改指到 `to` 底下的同一路径
fn remount_links(from: &Path, to: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for link in inbound_links_for(from) {
        let t = match fs::read_link(&link).map(|t| reg::strip_verbatim(&t)) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let nt = match remap_str(
            &t.to_string_lossy(),
            &from.to_string_lossy(),
            &to.to_string_lossy(),
        ) {
            Some(p) => PathBuf::from(p),
            None => continue,
        };
        if !swap::remove_link(&link) {
            out.push(format!("{} 的链接摘不掉，没动", link.display()));
            continue;
        }
        match swap::mklink_junction(&link, &nt) {
            Ok(_) if reg::is_reparse_point(&link) => out.push(format!(
                "{} → {}",
                link.file_name().unwrap_or_default().to_string_lossy(),
                nt.display()
            )),
            Ok(m) | Err(m) => out.push(format!("{} 重建链接失败：{}", link.display(), m)),
        }
    }
    out
}

fn running_agents() -> Vec<String> {
    let snap = crate::procs::snapshot();
    reg::all_apps_raw()
        .iter()
        .filter(|a| crate::procs::agent_running(&a.id, &snap))
        .map(|a| a.name.clone())
        .collect()
}

pub fn migrate(target_raw: &str) -> Value {
    let target = PathBuf::from(target_raw.trim().trim_matches('"').replace('/', "\\"));
    if let Err(e) = target_check(&target) {
        return json!({ "ok": false, "msg": e });
    }
    let old = resolve_root();
    if !old.exists() {
        return json!({ "ok": false, "msg": format!("现在的数据目录 {} 不存在", old.display()) });
    }
    let running = running_agents();
    if !running.is_empty() {
        return json!({
            "ok": false,
            "msg": format!(
                "还开着：{}。搬家要先关掉这些程序（它们握着账号目录的句柄），关掉后再点一次。",
                running.join("、")
            )
        });
    }

    let mut j = json!({
        "format": 1,
        "from": old.to_string_lossy(),
        "to": target.to_string_lossy(),
        "at": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        "status": "copying",
        "steps": []
    });
    write_journal(&j);

    let src = fingerprint(&old);
    let bytes: u64 = src.iter().map(|(_, b)| *b).sum();
    let free = free_gb_of(&target);
    if free < gb(bytes) + 2 {
        return json!({
            "ok": false,
            "msg": format!(
                "{}: 只剩 {} GB，这份数据 {} GB，至少要留出 2 GB 余量",
                drive_of(&target).unwrap_or_default(),
                free,
                (bytes as f64 / 1024.0 / 1024.0 / 1024.0 * 10.0).round() / 10.0
            )
        });
    }
    step(
        &mut j,
        "清点",
        &format!("{} 个条目 / {} MB", src.len(), bytes / 1024 / 1024),
    );

    if let Err(e) = copy_preserving(&old, &target) {
        j["status"] = json!("failed");
        step(&mut j, "复制失败", &e);
        return json!({
            "ok": false,
            "msg": format!("复制失败：{}\n原位置一份数据都没动，可以直接重试。", e)
        });
    }
    step(&mut j, "复制", &target.to_string_lossy());

    let old_s = old.to_string_lossy().to_string();
    let new_s = target.to_string_lossy().to_string();
    let fixed = rewrite_json_under(&target.join("instances"), &old_s, &new_s)
        + ["config.json", "inventory.json", "apikeys.json", "perks.json", "vault.json"]
            .iter()
            .map(|f| rewrite_json_at(&target.join(f), &old_s, &new_s))
            .sum::<usize>();
    step(&mut j, "改写路径", &format!("{} 处", fixed));

    let dst = fingerprint(&target);
    let missing: Vec<&String> = src
        .iter()
        .map(|(r, _)| r)
        .filter(|r| !dst.iter().any(|(d, _)| d == *r))
        .collect();
    if !missing.is_empty() {
        j["status"] = json!("failed");
        step(&mut j, "校验", &format!("缺 {} 个条目", missing.len()));
        return json!({
            "ok": false,
            "msg": format!(
                "校验没过：新位置缺 {} 个条目（例如 {}）。原位置完好、指针也还没切，删掉 {} 重试即可。",
                missing.len(),
                missing[0],
                target.display()
            )
        });
    }
    step(&mut j, "校验", &format!("逐条目一致（{} 项）", dst.len()));

    if let Err(e) = fs::write(pointer_file(), format!("{}\n", new_s)) {
        j["status"] = json!("failed");
        step(&mut j, "指针", &e.to_string());
        return json!({ "ok": false, "msg": format!("写指针文件失败：{}", e) });
    }
    j["status"] = json!("switched");
    step(&mut j, "切指针", &pointer_file().to_string_lossy());

    let remounted = remount_links(&old, &target);
    step(&mut j, "重挂入口", &remounted.join(" | "));

    let keep = old.with_file_name(format!(
        "{}{}",
        old.file_name().unwrap_or_default().to_string_lossy(),
        KEEPASIDE_SUFFIX
    ));
    let renamed = match fs::rename(&old, &keep) {
        Ok(_) => {
            step(&mut j, "留底", &keep.to_string_lossy());
            format!("原位置改名留底：{}", keep.display())
        }
        Err(e) => {
            step(&mut j, "留底", &format!("没改成：{}", e));
            format!("原位置还在 {}（改名失败：{}）", old.display(), e)
        }
    };

    j["status"] = json!("done");
    j["moved_mb"] = json!(bytes / 1024 / 1024);
    step(&mut j, "完成", &new_s);
    write_journal(&j);

    let dangling: Vec<String> = all_links()
        .iter()
        .filter(|(_, t)| !t.exists())
        .map(|(l, _)| l.to_string_lossy().into_owned())
        .collect();

    json!({
        "ok": true,
        "dangling": dangling,
        "msg": format!(
            "数据根已迁到 {}（{} MB，{} 个条目）。{}；{}。\n接下来把 WorkBuddy / TRAE / ZCode 各打开看一眼，对话和登录都在就说明搬对了。有异常去 设置→存储 点「退回原来的位置」，一步回到现在的样子。",
            new_s,
            bytes / 1024 / 1024,
            src.len(),
            renamed,
            if remounted.is_empty() {
                "没有需要改目标的入口链接".to_string()
            } else {
                format!("已把 {} 个入口改指新位置", remounted.len())
            }
        )
    })
}

pub fn rollback() -> Value {
    let j = match read_journal() {
        Some(j) => j,
        None => return json!({ "ok": false, "msg": "没有迁移记录，不用退" }),
    };
    let from = j["from"].as_str().map(PathBuf::from);
    let to = j["to"].as_str().map(PathBuf::from);
    let (Some(from), Some(to)) = (from, to) else {
        return json!({ "ok": false, "msg": "迁移记录里路径不全" });
    };
    let running = running_agents();
    if !running.is_empty() {
        return json!({ "ok": false, "msg": format!("还开着：{}，先全部关掉", running.join("、")) });
    }
    let keep = keepaside_path();
    if !from.exists() {
        match keep {
            Some(k) => {
                if let Err(e) = fs::rename(&k, &from) {
                    return json!({ "ok": false, "msg": format!("把留底改回原名失败：{}", e) });
                }
            }
            None => {
                return json!({
                    "ok": false,
                    "msg": format!(
                        "原位置 {} 和留底都不在了，没法退。只能继续在 {} 用。",
                        from.display(),
                        to.display()
                    )
                })
            }
        }
    }
    let _ = fs::remove_file(pointer_file());
    let remounted = remount_links(&to, &from);
    let mut nj = j.clone();
    nj["status"] = json!("rolled-back");
    step(&mut nj, "退回原位置", &from.to_string_lossy());
    json!({
        "ok": true,
        "remounted": remounted,
        "msg": format!(
            "已退回 {}（改了 {} 个入口）。新位置 {} 原样留着没删，确认原来的能用之后再手动删。",
            from.display(),
            remounted.len(),
            to.display()
        )
    })
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remap_only_whole_path_segments() {
        let old = r"C:\Users\x\.agent-cockpit";
        let new = r"D:\AgentData\.agent-cockpit";
        assert_eq!(
            remap_str(old, old, new).as_deref(),
            Some(r"D:\AgentData\.agent-cockpit")
        );
        assert_eq!(
            remap_str(r"C:/Users/x/.agent-cockpit/instances/a", old, new).as_deref(),
            Some(r"D:\AgentData\.agent-cockpit\instances\a"),
            "整串落成 Windows 规范形式，斜杠要统一"
        );
        assert_eq!(remap_str(r"C:\Users\x\.agent-cockpit-backup", old, new), None);
        assert_eq!(remap_str(r"D:\other\thing", old, new), None);
    }

    #[test]
    fn remap_value_walks_nested() {
        let mut v = json!({
            "swaps": { "workbuddy": { "original": r"C:\U\.ac\old" } },
            "list": [r"C:\U\.ac\a", "keepme"],
            "n": 7
        });
        assert_eq!(remap_value(&mut v, r"C:\U\.ac", r"D:\new"), 2);
        assert_eq!(v["swaps"]["workbuddy"]["original"], json!(r"D:\new\old"));
        assert_eq!(v["list"][1], json!("keepme"));
    }

    /// 搬家复制要连链接一起搬，逐条目一致；不能把 junction 解成实体目录
    #[test]
    fn copy_preserving_keeps_links() {
        let tmp = std::env::temp_dir().join("cockpit-migrate-copy");
        let _ = fs::remove_dir_all(&tmp);
        let src = tmp.join("root");
        let real = src.join("instances").join("app").join("账号1");
        fs::create_dir_all(real.join("sessions")).unwrap();
        fs::write(real.join("sessions").join("a.jsonl"), "chat").unwrap();
        fs::write(src.join("config.json"), "{}").unwrap();
        let link = src.join("dot-app");
        swap::mklink_junction(&link, &real);

        let dst = tmp.join("dst");
        copy_preserving(&src, &dst).unwrap();
        assert!(reg::is_reparse_point(&dst.join("dot-app")), "链接该按链接搬");
        assert_eq!(fs::read_to_string(dst.join("config.json")).unwrap(), "{}");
        let a = fingerprint(&src);
        let b = fingerprint(&dst);
        assert_eq!(a, b, "逐条目要一致");
        let _ = swap::remove_link(&link);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn fingerprint_counts_files_and_dirs() {
        let tmp = std::env::temp_dir().join("cockpit-fp");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("sub")).unwrap();
        fs::write(tmp.join("a.txt"), "12345").unwrap();
        fs::write(tmp.join("sub").join("b.txt"), "123").unwrap();
        assert_eq!(dir_bytes(&tmp), 8);
        let fp = fingerprint(&tmp);
        assert!(fp.iter().any(|(r, _)| r == "sub/"), "{:?}", fp);
        assert!(fp.iter().any(|(r, b)| r == "a.txt" && *b == 5));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn rewrite_json_fixes_stored_absolute_paths() {
        let tmp = std::env::temp_dir().join("cockpit-rewrite");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("instances").join("zcode")).unwrap();
        let inst = tmp.join("instances").join("zcode").join("账号1.json");
        fs::write(
            &inst,
            json!({ "name": "账号1", "data_dir": r"C:\U\.agent-cockpit\instances\zcode\账号1" })
                .to_string(),
        )
        .unwrap();
        let n = rewrite_json_under(&tmp, r"C:\U\.agent-cockpit", r"D:\AgentData\.agent-cockpit");
        assert_eq!(n, 1);
        let v: Value = serde_json::from_str(&fs::read_to_string(&inst).unwrap()).unwrap();
        assert_eq!(
            v["data_dir"],
            json!(r"D:\AgentData\.agent-cockpit\instances\zcode\账号1")
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    /// 目标盘必须是本地固定 NTFS、不能嵌套、不能是非空目录
    #[test]
    fn target_check_refuses_nonsense() {
        let cur = resolve_root();
        assert!(target_check(&cur).is_err(), "迁到自己头上要拒绝");
        assert!(target_check(Path::new("relative/path")).is_err());
        assert!(
            target_check(Path::new(&home())).is_err(),
            "非空目录/嵌套要拒绝"
        );
    }

    #[test]
    fn drives_report_at_least_the_system_drive() {
        let d = drives();
        assert!(!d.is_empty());
        assert!(d.iter().any(|v| v["drive"].as_str() == Some(&system_drive())));
        // 非系统盘排在前面
        if d.len() > 1 {
            assert_ne!(d[0]["drive"].as_str(), Some(system_drive().as_str()));
        }
    }
}
