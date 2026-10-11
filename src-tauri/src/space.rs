// 空间治理：占用统计 / 清缓存 / 账号备份恢复 / 多开去重
use crate::procs::{agent_running, snapshot};
use crate::registry::{
    self as reg, backup_dir, find_instance, load_config, save_instance, swaps_of, BACKUP_SKIP,
    CLEAN_DIRS,
};
use crate::storage;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 各产品可以放心**跨账号共用**的目录：程序自己下载的二进制、模型、虚拟机组件。
/// 多开一个号就整份复制一遍是最典型的浪费（WorkBuddy 单这一个目录就 3.3 GB）。
/// 只列可再生、且不含登录态/对话的路径——共用一份的后果最坏是程序重新下载一次。
/// 相对路径用 `/` 分隔，第一个账号搬进共享层，之后每个账号只留一个 junction。
pub const SHARED_SUBDIRS: &[(&str, &[&str])] = &[
    ("workbuddy", &["binaries"]),
    ("traework-cn", &["ModularData/ai-agent/vm"]),
];

pub fn share_list(app_id: &str) -> &'static [&'static str] {
    SHARED_SUBDIRS
        .iter()
        .find(|(a, _)| *a == app_id)
        .map(|(_, v)| *v)
        .unwrap_or(&[])
}

fn rel_norm(s: &str) -> String {
    s.replace('\\', "/").trim_end_matches('/').to_lowercase()
}

fn fingerprint_eq(a: &Path, b: &Path) -> bool {
    let x = storage::fingerprint(a);
    let y = storage::fingerprint(b);
    x.len() == y.len() && x.iter().zip(y.iter()).all(|(p, q)| p.0 == q.0 && p.1 == q.1)
}

fn link_into(mine: &Path, store: &Path) -> Result<(), String> {
    if let Some(p) = mine.parent() {
        let _ = fs::create_dir_all(p);
    }
    if reg::is_reparse_point(mine) {
        if !crate::swap::remove_link(mine) {
            return Err(format!("{} 的旧链接摘不掉", mine.display()));
        }
    } else if mine.exists() {
        let _ = fs::remove_dir_all(mine);
    }
    crate::swap::mklink_junction(mine, store)
        .map_err(|e| format!("建链接失败：{}", e))?;
    if !reg::is_reparse_point(mine) {
        return Err("建完链接没检测到 reparse".into());
    }
    Ok(())
}

/// 新账号里那些可再生大目录怎么安排：
/// ① 共享层还没有 → 搬过去（adopt，源已经在账号目录里）或从源复制一份（copy）；
/// ② 共享层已有且内容一致 → 账号里那份删掉，改成 junction；
/// ③ 内容对不上（版本不同）→ 各留各的，如实说明。
/// 返回（复制阶段可以跳过的相对路径，给人看的说明）。
fn stage_shared(app_id: &str, src: Option<&Path>, data_dir: &Path, moved: bool) -> (Vec<String>, Vec<String>) {
    let mut skip: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    if share_list(app_id).is_empty() || !cfg!(windows) {
        return (skip, notes);
    }
    for rel in share_list(app_id) {
        let mine = data_dir.join(rel.replace('/', "\\"));
        let store = storage::shared_dir(app_id).join(rel.replace('/', "\\"));
        let label = rel.rsplit('/').next().unwrap_or(rel);
        if moved {
            // 内容已经跟着整份档案进了账号目录，直接把这一坨挪进共享层
            if mine.exists() && !reg::is_reparse_point(&mine) {
                if store.exists() {
                    if fingerprint_eq(&mine, &store) {
                        match link_into(&mine, &store) {
                            Ok(_) => notes.push(format!("{} 与共享层一致，已改为共用一份", label)),
                            Err(e) => notes.push(format!("{} 没能共用：{}", label, e)),
                        }
                    } else {
                        notes.push(format!(
                            "{} 和共享层内容不一样（版本不同），这个账号保留自己那份",
                            label
                        ));
                    }
                } else if let Some(p) = store.parent() {
                    let _ = fs::create_dir_all(p);
                    match fs::rename(&mine, &store) {
                        Ok(_) => match link_into(&mine, &store) {
                            Ok(_) => notes.push(format!(
                                "{} 已挪进共享层，以后新建的账号都直接复用它",
                                label
                            )),
                            Err(e) => notes.push(format!("{} 挪走了但链接没建成：{}", label, e)),
                        },
                        Err(e) => notes.push(format!("{} 挪进共享层失败：{}", label, e)),
                    }
                }
                continue;
            }
            if !mine.exists() {
                // 账号目录里没有这坨（可能还没下载过）→ 有共享层就直接挂上
                if store.exists() {
                    if let Err(e) = link_into(&mine, &store) {
                        notes.push(format!("{} 挂共享层失败：{}", label, e));
                    }
                }
                continue;
            }
        }
        if let Some(s) = src {
            let from = s.join(rel.replace('/', "\\"));
            if !from.is_dir() || reg::is_reparse_point(&from) {
                continue;
            }
            if !store.exists() {
                let bytes = match copy_tree(&from, &store, &[]) {
                    Ok(b) => b,
                    Err(e) => {
                        notes.push(format!("{} 建共享层失败：{}", label, e));
                        continue;
                    }
                };
                notes.push(format!(
                    "{} 已建共享层（{} MB，只这一份，多个账号共用）",
                    label,
                    human_mb(bytes)
                ));
            } else {
                notes.push(format!("{} 直接复用共享层里那一份", label));
            }
            skip.push(rel_norm(rel));
        }
    }
    (skip, notes)
}

/// 给已经存在的账号补做去重（共享层机制上线之前建的号，各自揣着一份 binaries）
pub fn dedup_app(app_id: &str) -> Vec<String> {
    let mut out = Vec::new();
    if agent_running(app_id, &snapshot()) {
        out.push(format!("{} 还开着，先关掉再共用大文件", app_id));
        return out;
    }
    for m in reg::load_instances(app_id) {
        let dir = PathBuf::from(m["data_dir"].as_str().unwrap_or(""));
        if !dir.is_dir() {
            continue;
        }
        let name = m["name"].as_str().unwrap_or("?").to_string();
        let (_, notes) = stage_shared(app_id, None, &dir, true);
        for n in notes {
            out.push(format!("{}：{}", name, n));
        }
    }
    // 账号目录清空出来的共享层还没建的话，挑一个真有内容的号搬进去
    if out.is_empty() && !share_list(app_id).is_empty() {
        out.push("没有可共用的可再生大目录（或都已经共用了）".to_string());
    }
    out
}

pub fn dir_size(p: &Path) -> u64 {
    let mut total = 0u64;
    let rd = match fs::read_dir(p) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    for entry in rd.filter_map(|e| e.ok()) {
        let path = entry.path();
        let md = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if md.is_dir() {
            total += dir_size(&path);
        } else if md.is_file() {
            total += md.len();
        }
    }
    total
}

pub fn human_mb(bytes: u64) -> u64 {
    bytes / 1024 / 1024
}

/// 目录里有没有真东西（用来判断"这份档案是不是用户一直在用的那份"）
pub fn dir_has_data(p: &Path) -> bool {
    match fs::read_dir(p) {
        Ok(mut rd) => rd.next().is_some(),
        Err(_) => false,
    }
}

/// 复制一份档案（留着对话与记忆）。缓存目录跳过——可再生成，跳过能省一大截时间与空间。
/// junction/symlink 也跳过：既防环，也免得把别处的数据整份搬进来。
/// `skip_rel` 是已经决定走共享层的相对路径（归一化小写），复制时整块跳过，之后挂链接。
fn copy_tree(src: &Path, dst: &Path, skip: &[&str]) -> Result<u64, String> {
    copy_tree_rel(src, dst, skip, &[], "")
}

fn copy_tree_rel(
    src: &Path,
    dst: &Path,
    skip: &[&str],
    skip_rel: &[String],
    prefix: &str,
) -> Result<u64, String> {
    fs::create_dir_all(dst).map_err(|e| format!("建目录失败：{e}"))?;
    let rd = fs::read_dir(src).map_err(|e| format!("读不了 {}：{e}", src.display()))?;
    let mut total = 0u64;
    for e in rd.filter_map(|x| x.ok()) {
        let name = e.file_name();
        let name_s = name.to_string_lossy().to_string();
        if skip.iter().any(|s| s.eq_ignore_ascii_case(&name_s)) {
            continue;
        }
        let rel = if prefix.is_empty() {
            name_s.clone()
        } else {
            format!("{}/{}", prefix, name_s)
        };
        if skip_rel.contains(&rel_norm(&rel)) {
            continue;
        }
        let from = e.path();
        let to = dst.join(&name);
        let md = match fs::symlink_metadata(&from) {
            Ok(m) => m,
            Err(_) => continue,
        };
        const REPARSE: u32 = 0x400;
        #[cfg(windows)]
        let is_link = {
            use std::os::windows::fs::MetadataExt;
            md.file_attributes() & REPARSE != 0
        };
        #[cfg(not(windows))]
        let is_link = md.file_type().is_symlink();
        if is_link {
            continue;
        }
        if md.is_dir() {
            total += copy_tree_rel(&from, &to, skip, skip_rel, &rel)?;
        } else if md.is_file() {
            match fs::copy(&from, &to) {
                Ok(n) => total += n,
                Err(e) => return Err(format!("复制 {} 失败：{e}", name_s)),
            }
        }
    }
    Ok(total)
}

/// 新实例的数据来源。用户的原话是：「每一个先注册进 Cockpit 的账号实例，
/// 都应该是我本地之前一直在用的那个」——以前一律建空目录，于是登进去是个
/// 没有任何对话和记忆的新号（ZCode 就是这么中招的）。
///
/// ① `adopt`：**原地接管**——账号目录就是默认档案目录本身，零文件操作。
///    换号时由 swap 在同一父目录内把它改名让位（`xxx.cockpit-original`），随时换得回来。
///    早期版本把整个目录 rename 进 instances/，用户天天用的档案因此离开原位，
///    且 `swaps.original` 记成空串，「还原默认」只能还回一个空档案——别再那么干。
/// ② `copy` ：从另一份档案复制一份，保留对话与记忆；缓存目录跳过。
///    适合可多开的产品：**不动**用户原来那份，安全。
/// ③ `fresh`：空档。少数时候用户就是想要一个干净的窗口。
///
/// 返回 (实际用的来源, 给人看的一句说明)。
pub fn seed_instance(
    app_id: &str,
    data_dir: &Path,
    origin: &str,
    from: Option<&Path>,
) -> Result<(String, String), String> {
    if origin == "adopt" {
        let src = from.ok_or_else(|| "接管原有档案：没有可接管的目录".to_string())?;
        if data_dir != src {
            return Err(format!(
                "接管原有档案只登记目录本身，不再搬运：账号目录 {} 与源目录 {} 不一致",
                data_dir.display(),
                src.display()
            ));
        }
        if crate::registry::is_reparse_point(src) {
            return Err(format!(
                "{} 现在是一个换号链接（junction），不是原始档案——先点「还原默认」把它换回来，再接管",
                src.display()
            ));
        }
        return Ok((
            "adopt".into(),
            format!(
                "已沿用本机原有档案：这个账号的数据就在 {}，没有搬家，也没有复制",
                src.display()
            ),
        ));
    }
    if data_dir.exists() && dir_has_data(data_dir) {
        return Err(format!("账号目录 {} 里已经有数据了", data_dir.display()));
    }
    if origin == "fresh" {
        fs::create_dir_all(data_dir).map_err(|e| format!("建目录失败：{e}"))?;
        return Ok(("fresh".into(), "全新空档：这个账号打开时没有任何历史对话与记忆".into()));
    }
    let src = match from {
        Some(p) if p.is_dir() => p,
        _ => {
            fs::create_dir_all(data_dir).map_err(|e| format!("建目录失败：{e}"))?;
            return Ok((
                "fresh".into(),
                "没有找到可继承的档案，按全新空档创建".into(),
            ));
        }
    };
    // 目标已存在（空壳）时先让路：rename 要求目标不存在
    if data_dir.exists() {
        if dir_has_data(data_dir) {
            return Err(format!("账号目录 {} 里已经有数据了", data_dir.display()));
        }
        let _ = fs::remove_dir(data_dir);
    }
    // 走共享层的大目录整块跳过复制，复制完再挂链接——新账号只多一份登录态和对话
    let (skip, mut notes) = stage_shared(app_id, Some(src), data_dir, false);
    let bytes = copy_tree_rel(src, data_dir, CLEAN_DIRS, &skip, "")?;
    for rel in &skip {
        let mine = data_dir.join(rel.replace('/', "\\"));
        let store = storage::shared_dir(app_id).join(rel.replace('/', "\\"));
        if let Err(e) = link_into(&mine, &store) {
            notes.push(format!("{} 挂共享层失败：{}", rel, e));
        }
    }
    notes.insert(
        0,
        format!(
            "已从 {} 复制一份（{} MB，跳过缓存目录）：对话与记忆都带过来了，原档案没动",
            src.display(),
            human_mb(bytes)
        ),
    );
    Ok(("copy".into(), notes.join("；")))
}

fn walk_files(p: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = fs::read_dir(p) {
        for e in rd.filter_map(|x| x.ok()) {
            let path = e.path();
            match e.metadata() {
                Ok(md) if md.is_dir() => walk_files(&path, out),
                Ok(md) if md.is_file() => out.push(path),
                _ => {}
            }
        }
    }
}

pub fn clean_cache(app_id: &str, name: &str) -> (bool, String) {
    let meta = match find_instance(app_id, name) {
        Some(m) => m,
        None => return (false, "实例不存在".into()),
    };
    if agent_running(app_id, &snapshot()) {
        return (false, "请先关闭该实例窗口再清理".into());
    }
    let dir = PathBuf::from(meta["data_dir"].as_str().unwrap_or(""));
    if dir.as_os_str().is_empty() || !dir.exists() {
        return (false, "数据目录不存在".into());
    }
    let mut freed = 0u64;
    if let Ok(rd) = fs::read_dir(&dir) {
        for e in rd.filter_map(|x| x.ok()) {
            let path = e.path();
            let fname = e.file_name().to_string_lossy().to_string();
            let md = match e.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if md.is_dir() && CLEAN_DIRS.contains(&fname.as_str()) {
                freed += dir_size(&path);
                let _ = fs::remove_dir_all(&path);
            } else if md.is_file() && fname.ends_with(".log") {
                freed += md.len();
                let _ = fs::remove_file(&path);
            }
        }
    }
    (
        true,
        format!("已清理 {} MB 缓存（登录态与设置保留）", human_mb(freed)),
    )
}

pub fn backup(app_id: &str, name: &str) -> (bool, String) {
    let meta = match find_instance(app_id, name) {
        Some(m) => m,
        None => return (false, "实例不存在".into()),
    };
    let dir = PathBuf::from(meta["data_dir"].as_str().unwrap_or(""));
    if !dir.exists() {
        return (false, "数据目录不存在".into());
    }
    let _ = fs::create_dir_all(backup_dir());
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let zp = backup_dir().join(format!("{}__{}__{}.zip", app_id, name, ts));
    let file = match fs::File::create(&zp) {
        Ok(f) => f,
        Err(e) => return (false, format!("备份失败：{}", e)),
    };
    let mut zw = zip::ZipWriter::new(file);
    let opts = zip::write::FileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let mut files = Vec::new();
    walk_files(&dir, &mut files);
    let mut skipped = 0usize;
    for f in files {
        let rel = match f.strip_prefix(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if rel
            .components()
            .any(|c| BACKUP_SKIP.contains(&c.as_os_str().to_string_lossy().as_ref()))
        {
            skipped += 1;
            continue;
        }
        let name_in_zip = rel.to_string_lossy().replace('\\', "/");
        if zw.start_file(&name_in_zip, opts).is_err() {
            continue;
        }
        if let Ok(mut src) = fs::File::open(&f) {
            let mut buf = Vec::new();
            if src.read_to_end(&mut buf).is_ok() {
                let _ = zw.write_all(&buf);
            }
        }
    }
    match zw.finish() {
        Ok(_) => {
            let kb = fs::metadata(&zp).map(|m| m.len() / 1024).unwrap_or(0);
            (
                true,
                format!("已备份 {}（{} KB，跳过缓存 {} 个文件）", zp.display(), kb, skipped),
            )
        }
        Err(e) => {
            let _ = fs::remove_file(&zp);
            (false, format!("备份失败：{}", e))
        }
    }
}

pub fn restore(app_id: &str, name: &str) -> (bool, String) {
    let meta = match find_instance(app_id, name) {
        Some(m) => m,
        None => return (false, "实例不存在".into()),
    };
    let mut cands: Vec<PathBuf> = fs::read_dir(backup_dir())
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .map(|s| s.to_string_lossy().starts_with(&format!("{}__{}__", app_id, name)))
                        .unwrap_or(false)
                        && p.extension().map(|x| x == "zip").unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    cands.sort();
    let zp = match cands.pop() {
        Some(p) => p,
        None => return (false, "没有该实例的备份".into()),
    };
    let dir = PathBuf::from(meta["data_dir"].as_str().unwrap_or(""));
    let src = match fs::File::open(&zp) {
        Ok(f) => f,
        Err(e) => return (false, format!("恢复失败：{}", e)),
    };
    let mut ar = match zip::ZipArchive::new(src) {
        Ok(a) => a,
        Err(e) => return (false, format!("恢复失败：{}", e)),
    };
    // 先解到临时目录，成功后再替换，避免中途失败把实例目录搞坏
    let dirname = dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "instance".into());
    let staging = dir.with_file_name(format!("{}.restoring", dirname));
    let _ = fs::remove_dir_all(&staging);
    let _ = fs::create_dir_all(&staging);
    for i in 0..ar.len() {
        let mut f = match ar.by_index(i) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let rel = match f.enclosed_name() {
            Some(n) => n.to_path_buf(),
            None => continue,
        };
        let out = staging.join(rel);
        if f.is_dir() {
            let _ = fs::create_dir_all(&out);
            continue;
        }
        if let Some(parent) = out.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut w) = fs::File::create(&out) {
            let mut buf = Vec::new();
            let _ = f.read_to_end(&mut buf);
            let _ = w.write_all(&buf);
        }
    }
    if !staging.exists() {
        return (false, "恢复失败：解包为空".into());
    }
    let _ = fs::remove_dir_all(&dir);
    if let Err(e) = fs::rename(&staging, &dir) {
        // 这一步原目录已经删掉了，解包出来的那份是**唯一的副本**，绝不能顺手清掉 staging，
        // 必须把位置报清楚，让人能自己搬回去。
        return (
            false,
            format!(
                "恢复失败：{}。已解包出来的那份数据在 {}，先关掉该实例窗口再重试一次恢复即可；\
                 别手动删它，那是现在唯一的副本。",
                e,
                staging.display()
            ),
        );
    }
    let mut fresh = meta.clone();
    fresh["restored_at"] = serde_json::json!(chrono::Local::now().timestamp());
    save_instance(app_id, &fresh);
    (
        true,
        format!("已从 {} 恢复（请先关闭该实例窗口）", zp.file_name().unwrap_or_default().to_string_lossy()),
    )
}

pub fn open_dir(path: &str) -> (bool, String) {
    let p = PathBuf::from(path);
    let _ = fs::create_dir_all(&p);
    let mut c = std::process::Command::new("explorer");
    c.arg(&p);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    match c.spawn() {
        Ok(_) => (true, "已打开数据目录".into()),
        Err(e) => (false, format!("打开失败：{}", e)),
    }
}

/// 弹系统"选择文件夹"对话框，返回选中的路径。
/// 工作目录这种输入框光靠手打很难打对，给一个和资源管理器同款的选法。
/// 起始路径走环境变量传进去，避免把用户输入拼进命令行。
pub fn pick_folder(current: &str) -> (bool, String) {
    const PS: &str = r#"
$ErrorActionPreference = 'SilentlyContinue'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$owner = New-Object System.Windows.Forms.Form
$owner.TopMost = $true
$owner.ShowInTaskbar = $false
$owner.StartPosition = 'Manual'
$owner.Location = New-Object System.Drawing.Point(-32000, -32000)
$owner.Size = New-Object System.Drawing.Size(0, 0)
$owner.Show() | Out-Null
$dlg = New-Object System.Windows.Forms.FolderBrowserDialog
$d = $env:COCKPIT_PICK_TITLE
if ($d) { $dlg.Description = $d }
$dlg.ShowNewFolderButton = $true
$start = $env:COCKPIT_PICK_START
if ($start -and (Test-Path -LiteralPath $start)) { $dlg.SelectedPath = $start }
if ($dlg.ShowDialog($owner) -eq [System.Windows.Forms.DialogResult]::OK) {
  [Console]::Out.Write($dlg.SelectedPath)
}
$owner.Dispose()
"#;
    let mut c = std::process::Command::new("powershell");
    c.args(["-NoProfile", "-STA", "-Command", PS]);
    c.env("COCKPIT_PICK_START", current);
    // 提示文字走环境变量：嵌在源码里的那段 PowerShell 保持纯 ASCII，
    // 免得被控制台代码页二次编码成乱码（中文标题只在运行时注入）。
    c.env("COCKPIT_PICK_TITLE", "选择文件夹（只影响默认打开的项目位置）");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    match c.output() {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() {
                (false, "已取消".into())
            } else {
                (true, s)
            }
        }
        Err(e) => (false, format!("打不开选择对话框：{}", e)),
    }
}

/// 备份清单：每个 zip 的大小、时间、属于哪个 Agent/账号
pub fn list_backups() -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let rd = match fs::read_dir(backup_dir()) {
        Ok(r) => r,
        Err(_) => return out,
    };
    for e in rd.filter_map(|x| x.ok()) {
        let p = e.path();
        if p.extension().map(|x| x != "zip").unwrap_or(true) {
            continue;
        }
        let md = match e.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let stem = p
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut parts = stem.splitn(2, "__");
        let app = parts.next().unwrap_or("").to_string();
        let account = parts
            .next()
            .unwrap_or("")
            .split("__")
            .next()
            .unwrap_or("")
            .to_string();
        out.push(serde_json::json!({
            "file": p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            "app": app,
            "account": account,
            "bytes": md.len(),
            "mb": (md.len() as f64 / 1024.0 / 1024.0 * 10.0).round() / 10.0,
            "at": md.modified().ok()
                .and_then(|t| chrono::DateTime::<chrono::Local>::try_from(t).ok())
                .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default(),
        }));
    }
    out.sort_by(|a, b| b["file"].as_str().unwrap_or("").cmp(a["file"].as_str().unwrap_or("")));
    out
}

pub fn delete_backup(file: &str) -> (bool, String) {
    if file.is_empty()
        || file.contains("..")
        || file.contains('/')
        || file.contains('\\')
        || !file.ends_with(".zip")
    {
        return (false, "只能删除 backups 目录下的 zip 文件".into());
    }
    let p = backup_dir().join(file);
    if !p.exists() {
        return (false, "文件不存在".into());
    }
    match fs::remove_file(&p) {
        Ok(_) => (true, format!("已删除备份 {}", file)),
        Err(e) => (false, format!("删除失败：{}", e)),
    }
}

/// 账号改名：同步改数据目录、档案 json 与换号记录
pub fn rename_instance(app_id: &str, from: &str, to: &str) -> (bool, String) {
    let to = to.trim();
    if to.is_empty() {
        return (false, "新名字不能为空".into());
    }
    if to.contains("..") || to.contains('/') || to.contains('\\') || to.contains(':') {
        return (false, "名字不能含路径分隔符".into());
    }
    if to == from {
        return (true, "名字没变".into());
    }
    let mut meta = match find_instance(app_id, from) {
        Some(m) => m,
        None => return (false, "账号不存在".into()),
    };
    // 只有"这个账号正被 junction 挂成默认档案"时，把数据目录挪走才会留下断链；
    // 那也只是不让挪目录，昵称照样该让人改（见下面的 movable）
    let swapped_here = swaps_of(&load_config())
        .get(app_id)
        .and_then(|s| s["instance"].as_str())
        == Some(from);
    let old_dir = PathBuf::from(meta["data_dir"].as_str().unwrap_or(""));
    let new_dir = match old_dir.file_name() {
        Some(_) => old_dir.with_file_name(to),
        None => return (false, "数据目录路径异常".into()),
    };
    // 改昵称动的只是驾驶舱自己那条记录，任何时候都行；把数据目录一起改名才是"要看时机"的
    // 操作（Windows 不许改名正被程序打开的目录，junction 指着它时挪走还会留断链）。
    // 所以目录能顺手改就改，改不动就留着旧路径说清楚——别为了目录名拒绝人改昵称。
    let mut dir_note = String::new();
    if old_dir != new_dir {
        if new_dir.exists() {
            return (false, format!("已存在同名账号目录「{}」，先换个名字", to));
        }
        if swapped_here {
            dir_note = "\n（数据目录名没跟着改：这个账号正被挂成默认档案，先「还原默认」再改一次名字就会同步）".to_string();
        } else if agent_running(app_id, &snapshot()) {
            dir_note = "\n（数据目录名没跟着改：程序还在跑；关掉后再改一次名字就同步）".to_string();
        } else if old_dir.exists() {
            match fs::rename(&old_dir, &new_dir) {
                Ok(_) => meta["data_dir"] = serde_json::json!(new_dir.to_string_lossy()),
                Err(e) => dir_note = format!("\n（数据目录没改成：{}）", e),
            }
        }
    }
    meta["name"] = serde_json::json!(to);
    save_instance(app_id, &meta);
    if let Some(f) = meta["_file"].as_str() {
        let _ = fs::remove_file(f);
    }
    let mut cfg = crate::registry::load_config();
    if let Some(s) = cfg.get_mut("swaps").and_then(|v| v.get_mut(app_id)) {
        if s["instance"].as_str() == Some(from) {
            s["instance"] = serde_json::json!(to);
        }
    }
    crate::registry::save_config(&cfg);
    (
        true,
        format!("已把「{}」改名为「{}」{}", from, to, dir_note),
    )
}

/// 所有配了共享层的产品都过一遍
pub fn dedup_all() -> Vec<String> {
    SHARED_SUBDIRS
        .iter()
        .flat_map(|(a, _)| dedup_app(a))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(name);
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// 接管（adopt）＝原地登记：数据留在默认档案目录里，一个文件都不搬。
    /// 早期版本把它 rename 进 instances/，用户天天用的档案因此离开原位，
    /// 「还原默认」还没有退路——这条测试就是钉死别再犯。
    #[test]
    fn adopt_registers_the_profile_in_place_without_moving() {
        let root = tmp("cockpit-seed-adopt");
        let src = root.join("dot-zcode");
        fs::create_dir_all(src.join("sessions")).unwrap();
        fs::write(src.join("history.jsonl"), "hello").unwrap();
        fs::write(src.join("sessions").join("a.json"), "chat").unwrap();

        let (used, note) = seed_instance("codex", &src, "adopt", Some(&src)).unwrap();
        assert_eq!(used, "adopt");
        assert!(src.exists(), "原地接管不许把档案目录弄没");
        assert!(src.join("history.jsonl").is_file(), "内容必须原地不动");
        assert!(note.contains("没有搬家"), "说明该讲清不搬：{note}");

        // 想搬走？直接报错，不许留第二条路
        let dst = root.join("instances").join("账号1");
        assert!(seed_instance("codex", &dst, "adopt", Some(&src)).is_err());
        assert!(!dst.exists(), "拒绝的时候就一个字节都不该动");
        let _ = fs::remove_dir_all(&root);
    }

    /// 复制（copy）：原档案必须原地不动，缓存目录跳过
    #[test]
    fn copy_keeps_the_original_and_skips_caches() {
        let root = tmp("cockpit-seed-copy");
        let src = root.join("dot-codex");
        fs::create_dir_all(src.join("sessions")).unwrap();
        fs::create_dir_all(src.join("Cache")).unwrap();
        fs::create_dir_all(src.join("logs")).unwrap();
        fs::write(src.join("auth.json"), "{}").unwrap();
        fs::write(src.join("sessions").join("s.jsonl"), "chat").unwrap();
        fs::write(src.join("Cache").join("big.bin"), "x".repeat(100)).unwrap();
        fs::write(src.join("logs").join("a.log"), "noise").unwrap();
        let dst = root.join("instances").join("账号2");

        let (used, note) = seed_instance("codex", &dst, "copy", Some(&src)).unwrap();
        assert_eq!(used, "copy");
        assert!(src.join("auth.json").is_file(), "原档案不能被动");
        assert!(fs::read_to_string(dst.join("sessions").join("s.jsonl")).unwrap() == "chat");
        assert!(dst.join("auth.json").is_file(), "登录状态要带过来");
        assert!(!dst.join("Cache").exists(), "缓存目录该跳过");
        assert!(!dst.join("logs").exists(), "日志该跳过");
        assert!(note.contains("原档案没动"), "说明该说清原档案没动：{note}");
        let _ = fs::remove_dir_all(&root);
    }

    /// 全新空档：目录建好、里面是空的；这是"想要一个干净窗口"时的选项
    #[test]
    fn fresh_makes_an_empty_dir() {
        let root = tmp("cockpit-seed-fresh");
        let dst = root.join("账号3");
        let src = root.join("dot-x");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("history"), "should not be copied").unwrap();
        let (used, note) = seed_instance("codex", &dst, "fresh", Some(&src)).unwrap();
        assert_eq!(used, "fresh");
        assert!(dst.is_dir());
        assert!(!dir_has_data(&dst), "空档就该是空的：{note}");
        let _ = fs::remove_dir_all(&root);
    }

    /// 找不到可继承的档案时要退化成空档，而不是报错卡住流程
    #[test]
    fn missing_source_falls_back_to_fresh() {
        let root = tmp("cockpit-seed-nosrc");
        let dst = root.join("账号4");
        let (used, note) = seed_instance("codex", &dst, "copy", Some(&root.join("不存在"))).unwrap();
        assert_eq!(used, "fresh");
        assert!(note.contains("全新空档"), "{note}");
        let _ = fs::remove_dir_all(&root);
    }

    /// 多开去重：可再生大目录只留一份，账号目录里挂链接；对话与登录照常各一份
    #[test]
    fn heavy_dirs_are_shared_not_copied() {
        let root = tmp("cockpit-seed-shared");
        storage::set_shared_override(Some(root.join("shared")));
        let src = root.join("dot-workbuddy");
        fs::create_dir_all(src.join("binaries")).unwrap();
        fs::write(src.join("binaries").join("agent.bin"), "x".repeat(2048)).unwrap();
        fs::create_dir_all(src.join("sessions")).unwrap();
        fs::write(src.join("sessions").join("s.jsonl"), "chat").unwrap();

        let a = root.join("accounts").join("账号1");
        let (used, note) = seed_instance("workbuddy", &a, "copy", Some(&src)).unwrap();
        assert_eq!(used, "copy");
        assert!(
            reg::is_reparse_point(&a.join("binaries")),
            "binaries 该挂成共享层链接：{}",
            note
        );
        assert_eq!(fs::read_to_string(a.join("sessions").join("s.jsonl")).unwrap(), "chat");
        assert!(src.join("binaries").join("agent.bin").is_file(), "源档案不能动");
        assert!(note.contains("共享层"), "说明要讲清共用了：{}", note);
        let store = storage::shared_dir("workbuddy").join("binaries");
        assert!(store.join("agent.bin").is_file());

        // 第二个号：共享层已有 → 不再复制第二份字节
        let b = root.join("accounts").join("账号2");
        let (_, note2) = seed_instance("workbuddy", &b, "copy", Some(&src)).unwrap();
        assert!(reg::is_reparse_point(&b.join("binaries")), "{}", note2);
        assert_eq!(storage::fingerprint(&store).len(), 1, "共享层始终只有一份 binaries");

        // 第三个号想要干净的：清空链接不能误删共享层那份
        let _ = crate::swap::remove_link(&b.join("binaries"));
        let _ = fs::remove_dir_all(&b.join("binaries"));
        assert!(store.join("agent.bin").is_file(), "删账号目录里那份链接不能吃掉共享层");
        storage::set_shared_override(None);
        let _ = fs::remove_dir_all(&root);
    }

    /// 只有登记过的产品才有共用目录，别把不认识的结构乱挂链接
    #[test]
    fn only_listed_apps_share_anything() {
        assert_eq!(share_list("workbuddy"), &["binaries"][..]);
        assert_eq!(share_list("codex"), &[] as &[&str]);
        assert_eq!(share_list("qoder-cn"), &[] as &[&str]);
    }

    /// 已有数据的目录不许被覆盖——接管/复制都不能吃掉别的东西
    #[test]
    fn refuses_to_overwrite_a_dir_with_data() {
        let root = tmp("cockpit-seed-nonoverwrite");
        let src = root.join("dot-y");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a"), "1").unwrap();
        let dst = root.join("账号5");
        fs::create_dir_all(&dst).unwrap();
        fs::write(dst.join("mine"), "keep me").unwrap();
        assert!(seed_instance("codex", &dst, "adopt", Some(&src)).is_err());
        assert!(seed_instance("codex", &dst, "copy", Some(&src)).is_err());
        assert_eq!(fs::read_to_string(dst.join("mine")).unwrap(), "keep me");
        let _ = fs::remove_dir_all(&root);
    }
}
